# AniPulse 网页管理端实施计划

## 1. 目标与结论

网页管理端服务于单个自托管管理员，目标是把常用 CLI 操作变成安全、可审计的浏览器操作，同时不削弱现有“低频请求、保守确认、通知幂等”的边界。

推荐方案：

- 使用 Rust + Axum 构建独立的 `anipulse web` 进程；
- 使用服务端渲染 HTML、少量本地静态资源，不引入 Node.js 运行时或公开 SPA API；
- 网页进程只监听 `127.0.0.1`，由 Caddy/Nginx 提供 HTTPS，或者仅通过 Tailscale/WireGuard 私网访问；
- V1 使用内置单管理员账号、Argon2id 密码哈希和服务端不透明 Session；
- 所有写操作都经过鉴权、CSRF 校验、Origin 校验和审计；
- 网页与调度器是两个 systemd 服务，共享 SQLite，但网页不读取飞书 Secret；
- 网页触发的外部检查/同步通过数据库任务交给现有调度器执行，避免两个进程同时请求 Bilibili、Bangumi 或飞书；
- CLI 保持可用，作为网页故障、密码重置和灾难恢复入口。

本计划不在第一版引入公开注册、多租户、复杂角色系统、OAuth 社交登录、移动 App、WebSocket 或公网开放的通用 API。

## 2. 总体架构

```text
浏览器
  │ HTTPS
  ▼
Caddy / Nginx / Tailscale HTTPS
  │ 仅转发到 127.0.0.1:8080
  ▼
anipulse web（无飞书 Secret）
  │
  ├── Auth / Session / CSRF / HTML
  ├── ApplicationService（统一业务规则）
  └── SQLite：管理数据、Session、审计、任务
             ▲
             │ 领取 management_job
anipulse run（现有监控进程，持有外部服务凭证）
  ├── Bilibili / Bangumi
  └── 飞书通知
```

选择独立网页进程而不是把 HTTP Server 塞进 `anipulse run`，有三个原因：

1. 网页重启或模板错误不会停止番剧监控；
2. 网页服务不需要读取 `FEISHU_APP_SECRET`，权限更小；
3. 可以单独限制网页进程的网络、资源、日志和 systemd 权限。

两个进程共享 SQLite 前，需要开启 WAL、保留 `busy_timeout`、限制连接池规模，并让数据库迁移在部署阶段显式执行，避免同时启动时竞争 migration。

## 3. 威胁模型与信任边界

### 3.1 需要防护

- 公网扫描、密码猜测和账号枚举；
- Session 猜测、窃取、固定和重放；
- CSRF 导致的删除、确认候选或修改追番；
- XSS 窃取页面数据或代替管理员操作；
- 伪造 `X-Forwarded-For` 绕过登录限流；
- URL、日志、浏览器存储或错误页面泄漏 Session/密码/Secret；
- 错误 Bangumi ID、错误 Anime ID 或重复提交造成的数据破坏；
- 网页进程被利用后直接读取飞书 Secret 或高频调用外部 Provider；
- 删除与调度器并发时产生过期推送；
- SQLite 锁竞争导致监控主流程失效。

### 3.2 信任假设

- Linux 主机、root、`anipulse` 系统用户和反向代理配置可信；
- `/etc/anipulse/*.env` 权限保持 `0600`，SQLite 目录不对其他系统用户开放；
- 浏览器和管理员终端可信；
- HTTPS 终止点可信，HTTP 不直接暴露到公网；
- Bilibili、Bangumi 和飞书仍属于不稳定外部依赖，网页不能把它们的响应当成更新确认依据。

## 4. 技术选型

### 4.1 HTTP 与模板

- Axum：复用项目现有 Tokio 运行时，通过 Router、Extractor 和 Tower middleware 组织路由、鉴权、超时和日志；
- 服务端模板：优先选择编译期或默认自动转义的 Rust 模板库；禁止对用户/外部数据使用 raw HTML；
- `tower-http`：请求追踪、超时、请求体限制、安全响应头和静态资源；
- CSS 与少量 JavaScript 随二进制或本地静态目录发布，不从第三方 CDN 加载；
- V1 使用普通 HTML form/POST/Redirect/GET，不依赖前端框架。

### 4.2 为什么不先做 SPA

- 个人管理端页面少，服务端渲染能够显著减少 CORS、Token 存储和前端构建链复杂度；
- 不需要把长期 Token 放进 `localStorage`；
- 删除、确认、禁用等动作天然适合表单 + CSRF；
- 服务器只有约 1.7 GiB 内存，无 Node 运行时更容易部署和排障。

### 4.3 业务层重构

当前 CLI 直接组合 Repository、ScheduleProvider 和 Detector。网页实施前先引入 `ApplicationService`，让 CLI 与网页共用以下用例：

- 添加、查看、启用、禁用、删除 Anime；
- 解析并确认自动排期；
- 接受/拒绝候选；
- 信任/屏蔽 UP；
- 请求立即检查、立即同步和通知测试；
- 读取 Dashboard 状态与审计记录。

HTTP handler 不直接写 SQL，也不复制 CLI 的验证逻辑。Repository 只负责持久化，ApplicationService 负责业务约束和事务边界。

## 5. 鉴权设计

### 5.1 账号模型

V1 只允许一个 owner 管理员，不开放注册页面。管理员必须通过服务器终端创建：

```bash
sudo -u anipulse /usr/local/bin/anipulse \
  --config /etc/anipulse/config.toml \
  auth admin create --username admin
```

命令从 TTY 隐藏输入密码并要求重复确认，禁止通过命令行参数或环境变量传入明文密码。还需要提供：

```text
auth admin reset-password --username admin
auth sessions revoke-all --username admin
auth admin disable --username admin
```

密码重置和账号禁用必须立即撤销该账号的所有 Session。不存在管理员时，网页只能返回初始化提示，不能创建默认密码。

### 5.2 密码存储

- 使用 RustCrypto `argon2` 的 Argon2id 和随机独立 salt；
- 保存标准 PHC 字符串，让算法、版本和 work factor 随 hash 一起保存；
- 参数至少达到 OWASP 当前 Argon2id 基线，并在目标 2 vCPU/1.7 GiB 服务器上校准到合理登录耗时；
- 密码哈希放入 `spawn_blocking`，并用全局 semaphore 将并发哈希限制为 1，避免并发登录拖垮小内存服务器；
- 限制密码输入长度，避免超大请求造成不必要的哈希开销；
- 用户不存在时也执行一次 dummy Argon2 校验，减少账号枚举时序差异；
- 登录错误统一显示“用户名或密码错误”，日志也不记录密码。

### 5.3 Session

- 登录成功后由操作系统 CSPRNG 生成至少 256-bit 随机不透明 Token；
- 浏览器只保存随机 Token，不保存用户信息或权限；
- SQLite 只保存 Token 的 keyed HMAC，不保存原始 Token；
- 使用 32-byte 以上 `ANIPULSE_WEB_SECRET`，通过 HKDF 派生 Session、CSRF 和审计哈希的独立 key；
- Cookie 名使用 `__Host-anipulse_session`；
- Cookie 固定设置 `Secure; HttpOnly; SameSite=Lax; Path=/`，不设置 `Domain`；
- 默认 idle timeout 2 小时、absolute timeout 24 小时、renewal timeout 30 分钟，全部允许在安全范围内配置；
- 登录时创建新 Session；续期时轮换 Token；退出、过期、改密和账号禁用时服务端立即撤销；
- 登录页和所有鉴权页面返回 `Cache-Control: no-store`；
- Session 只接受 Cookie，不接受 URL 参数、表单字段、Authorization query 或 localStorage。

### 5.4 CSRF 与请求来源

- 所有状态修改只允许 POST/PUT/DELETE；GET/HEAD 永远无副作用；
- HTML 表单携带与当前 Session 绑定的 HMAC CSRF Token；
- JSON 请求必须包含自定义 `X-CSRF-Token`；
- 同时校验 `Origin`，缺失时严格校验 `Referer`；两者都缺失的写请求默认拒绝；
- Cookie SameSite 只是附加保护，不能代替 CSRF Token；
- 默认不启用 CORS，也不返回 `Access-Control-Allow-Credentials`；
- 删除、接受候选等高影响动作使用一次性 action nonce，成功或失败后失效，防止重复提交。

### 5.5 登录限流

持久化记录“规范化用户名 + 有效来源 IP”的 keyed hash：

- 默认 15 分钟窗口最多 5 次失败；
- 超限后指数增加等待时间，但设最大值，避免永久锁死；
- 同时设置用户名维度和来源 IP 维度限制；
- 成功登录清理对应失败计数；
- 只有请求确实来自配置的可信反向代理时才读取 `Forwarded`/`X-Forwarded-For`；
- 其他请求一律使用 TCP peer address，防止伪造头部绕过限流。

### 5.6 授权中间件

仅以下路由匿名可访问：

- `GET /login`
- `POST /login`
- `GET /healthz`，且只返回固定的健康状态，不泄漏版本、数据库路径、Anime 数量或外部服务错误

其他 HTML 与 `/api/v1` 路由统一挂载 Session middleware。V1 虽然只有 owner，也保留明确的 `role=owner` 数据列和权限检查，不把“登录成功”散落等同于授权。

## 6. 安全响应头与页面约束

默认由应用和反向代理共同设置并测试：

```text
Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data: https://i0.hdslb.com https://i1.hdslb.com https://i2.hdslb.com; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'
X-Content-Type-Options: nosniff
Referrer-Policy: strict-origin-when-cross-origin
Permissions-Policy: camera=(), microphone=(), geolocation=()
Cross-Origin-Opener-Policy: same-origin
Cache-Control: no-store
```

HSTS 只在域名和 HTTPS 证书稳定后由反向代理启用。模板默认转义所有 Anime 标题、候选标题、UP 名、外部错误和审计 metadata。外部 URL 只允许预先定义的 Bilibili/Bangumi 页面格式，不能把数据库中的任意字符串直接放入链接或重定向。

## 7. 数据库设计

新增 migration，不修改现有业务表含义。建议表结构如下，最终以 SQL migration 和约束为准。

### 7.1 管理员

```sql
CREATE TABLE web_admin (
    id INTEGER PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    role TEXT NOT NULL CHECK(role IN ('owner')),
    disabled INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    password_changed_at TEXT NOT NULL,
    last_login_at TEXT
);
```

用户名需要规范化并限制字符集/长度，避免视觉混淆和重复账号。

### 7.2 Session

```sql
CREATE TABLE web_session (
    token_hmac BLOB PRIMARY KEY,
    admin_id INTEGER NOT NULL REFERENCES web_admin(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL,
    renewed_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    idle_expires_at TEXT NOT NULL,
    absolute_expires_at TEXT NOT NULL,
    user_agent_hash BLOB,
    source_ip_hash BLOB,
    revoked_at TEXT
);
```

建立过期索引并由网页进程定期批量清理。日志只记录 Session 的不可逆关联 hash 前缀，绝不打印 Cookie 或完整 HMAC。

### 7.3 登录限流

```sql
CREATE TABLE auth_throttle (
    key_hmac BLOB PRIMARY KEY,
    window_started_at TEXT NOT NULL,
    failure_count INTEGER NOT NULL,
    blocked_until TEXT,
    updated_at TEXT NOT NULL
);
```

### 7.4 审计

```sql
CREATE TABLE audit_event (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    actor_type TEXT NOT NULL,
    actor_admin_id INTEGER REFERENCES web_admin(id) ON DELETE SET NULL,
    action TEXT NOT NULL,
    entity_type TEXT,
    entity_id TEXT,
    outcome TEXT NOT NULL,
    request_id TEXT,
    source_ip_hash BLOB,
    metadata_json TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL
);
```

审计覆盖登录成功/失败、退出、改密、Session 撤销、添加/删除/启停追番、候选确认/拒绝、UP 信任变更和任务触发。metadata 使用字段 allowlist，不记录密码、Cookie、CSRF、飞书 Secret 或完整外部响应。

### 7.5 管理任务

```sql
CREATE TABLE management_job (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    target_type TEXT,
    target_id TEXT,
    state TEXT NOT NULL,
    requested_by INTEGER REFERENCES web_admin(id) ON DELETE SET NULL,
    dedupe_key TEXT,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    error TEXT
);
```

`anipulse run` 原子领取 `queued` 任务，支持 `check_anime`、`sync_schedule`、`notification_test`。相同目标的未完成任务使用 dedupe key 去重；失败保存简化错误并允许受控重试。网页只入队，不直接调用外部 Provider。

## 8. 页面与路由

### 8.1 页面路由

| 方法 | 路由 | 鉴权 | 用途 |
|---|---|---:|---|
| GET | `/login` | 否 | 登录页 |
| POST | `/login` | 否 | 登录、限流、创建 Session |
| POST | `/logout` | 是 | 撤销当前 Session |
| GET | `/` | 是 | Dashboard |
| GET | `/anime` | 是 | 追番列表、状态和下一次检查 |
| GET | `/anime/new` | 是 | 添加向导第一步 |
| POST | `/anime/resolve` | 是 | 解析 Bangumi 候选，创建短期 draft |
| POST | `/anime` | 是 | 用户确认 draft 后创建 Anime |
| GET | `/anime/{id}` | 是 | 详情、别名、当前 Episode、同步状态 |
| POST | `/anime/{id}/enable` | 是 | 启用 |
| POST | `/anime/{id}/disable` | 是 | 禁用 |
| POST | `/anime/{id}/delete` | 是 | 永久删除，要求输入标题确认 |
| POST | `/anime/{id}/check` | 是 | 入队立即检查 |
| POST | `/anime/{id}/sync` | 是 | 入队自动排期同步 |
| GET | `/candidates` | 是 | 候选分页、分数解释 |
| POST | `/candidates/{bvid}/accept` | 是 | 人工确认 |
| POST | `/candidates/{bvid}/reject` | 是 | 人工拒绝 |
| POST | `/anime/{id}/uploaders/{mid}/trust` | 是 | 信任 UP |
| POST | `/anime/{id}/uploaders/{mid}/block` | 是 | 屏蔽 UP |
| GET | `/jobs` | 是 | 后台任务状态 |
| GET | `/audit` | 是 | 审计记录 |
| GET | `/settings/status` | 是 | 脱敏配置与外部服务状态 |

V1 页面使用 POST/Redirect/GET，刷新结果页不会重复执行写操作。列表必须分页并限制 page size，过滤条件使用枚举 allowlist。

### 8.2 添加追番向导

添加操作分为两次明确提交：

1. 输入标题、等待集数、时长范围、时区以及是否自动排期；
2. 服务端解析 Bangumi，页面显示“输入标题、匹配标题、Bangumi ID、别名、预计时间”；
3. 用户确认正确匹配后才创建记录。

解析结果存入与 Session 绑定、15 分钟过期的 server-side draft。最终请求只引用 draft ID，不能由浏览器篡改 subject ID 或别名。显式指定 Bangumi ID 但标题相似度很低时显示强警告，并要求额外确认；默认不静默接受。这直接防止把“无职转生第三季”绑定到《恶女不才》ID 的错误。

### 8.3 删除交互

删除页显示 Anime ID、标题、Bangumi ID、当前 Episode，以及将被级联删除的候选/通知数量。管理员必须：

1. 先禁用目标；
2. 输入完整 Anime 标题；
3. 提交一次性 action nonce；
4. 服务端重新加载记录，并检查 `updated_at`，避免页面打开后目标发生变化；
5. 在同一事务内删除业务数据并写入审计摘要。

网页不提供“批量删除”。删除成功后跳转列表并显示不可撤销提示；备份恢复是唯一撤销方式。

### 8.4 JSON API

V1 不对第三方开放 API。页面需要少量异步交互时，可在同源 `/api/v1` 下提供内部 JSON endpoint，并与 HTML 路由复用相同 Session、CSRF、授权和 ApplicationService。默认关闭 CORS，错误结构不返回内部 SQL、路径或外部 Secret。

## 9. 进程通信与并发

- Web 直接执行纯 SQLite 管理事务，如启停、删除、候选 accept/reject；
- 需要 Bilibili、Bangumi 或飞书访问的操作写入 `management_job`；
- Scheduler 每个 tick 领取有限数量任务，再执行现有 Detector/Schedule/Notification 逻辑；
- 任务领取使用事务状态变更，确保两个 scheduler 实例不会重复执行；
- Web SQLite pool 建议最多 4 个连接，密码哈希不占数据库连接等待；
- 开启 WAL 和合理 `busy_timeout`，对 `SQLITE_BUSY` 映射为可重试的 503/页面提示；
- 删除前禁用 Anime，Scheduler 领取任务后再次验证 Anime 仍存在且 enabled；
- Candidate accept 和通知创建维持现有唯一约束，重复点击仍然幂等。

## 10. 配置与 Secret

建议新增：

```toml
[web]
bind = "127.0.0.1:8080"
public_url = "https://anime.example.com"
trusted_proxy_cidrs = ["127.0.0.1/32", "::1/128"]
session_idle_secs = 7200
session_absolute_secs = 86400
session_renewal_secs = 1800
login_window_secs = 900
login_max_failures = 5
request_timeout_secs = 15
max_body_bytes = 65536
```

`ANIPULSE_WEB_SECRET` 只存在于 `/etc/anipulse/anipulse-web.env`：

```text
ANIPULSE_WEB_SECRET=base64-encoded-at-least-32-random-bytes
RUST_LOG=info
```

启动时必须校验：

- Secret 解码后长度足够；
- `public_url` 是 HTTPS，只有显式开发模式允许 localhost HTTP；
- bind 默认是 loopback；绑定 `0.0.0.0` 必须显式开启危险选项并打印高优先级告警；
- trusted proxy 不能默认信任任意地址；
- Session timeout、请求体大小和登录限流必须在安全范围。

飞书 App Secret 继续只放在 scheduler 的 `/etc/anipulse/anipulse.env`，不能复制到 web 环境文件，也不能在设置页编辑或显示。

## 11. 部署设计

### 11.1 systemd

新增 `anipulse-web.service`，与现有 `anipulse.service` 分开：

- `User=anipulse`、`Group=anipulse`；
- `ExecStart=/usr/local/bin/anipulse --config /etc/anipulse/config.toml web`；
- 只加载 `anipulse-web.env`；
- `NoNewPrivileges=true`、`PrivateTmp=true`、`ProtectSystem=strict`、`ProtectHome=true`；
- `ReadWritePaths=/var/lib/anipulse`；
- 限制地址族和能力；
- 设置内存、文件描述符和重启策略；
- scheduler 与 web 都在 migration 成功后启动。

增加显式命令：

```bash
anipulse database migrate
```

部署流程先备份数据库，再停止两个服务，运行 migration，启动 scheduler，最后启动 web。旧 scheduler 应能忽略新增表，因此回滚时可以先关闭 web，再恢复旧二进制；涉及不可逆 migration 时必须恢复升级前备份。

### 11.2 反向代理

默认推荐：

```text
anime.example.com:443 -> Caddy -> 127.0.0.1:8080
```

要求：

- 自动 HTTPS 或明确配置可信证书；
- HTTP 永久跳转 HTTPS；
- 限制请求体和上游超时；
- 不缓存鉴权页面和 `Set-Cookie`；
- 覆盖/重建 Forwarded headers，不透传客户端伪造值；
- 防火墙只开放 SSH 和 443，8080 不对公网监听；
- 如果仅个人设备使用，优先考虑 Tailscale HTTPS/私网 DNS，进一步减少公网攻击面。

## 12. 可观测性与审计

- 每个请求生成 request ID，并在结构化日志和审计记录中关联；
- 日志包含路由模板、状态码、延迟和错误类别，不打印 form body、Cookie、Authorization、CSRF 或完整 query；
- 登录失败只记录 username keyed hash、来源 keyed hash和原因类别；
- 审计页分页显示高影响操作，默认保留 180 天，可配置归档；
- `/healthz` 只检查进程可响应；详细数据库、任务、Provider 和通知状态只能登录后查看；
- Dashboard 显示 scheduler 最后心跳、任务积压、失败通知数和 Provider backoff，但不显示 Secret。

## 13. 分阶段交付

### 阶段 0：CLI 安全删除

交付：

- `anime remove ID --yes`；
- 默认拒绝未确认删除；
- SQLite 外键级联测试；
- README 与部署文档。

验收：删除不存在 ID 返回 NotFound；不带 `--yes` 不改变数据；删除后 Anime、Alias、Episode、Candidate、Notification、UploaderTrust 全部消失；Provider 全局状态不受影响。

### 阶段 1：应用服务与数据库并发基础

交付：

- `ApplicationService`，CLI 改为调用统一用例；
- WAL、连接池和 busy timeout 配置；
- `database migrate`；
- `management_job` 和 scheduler 领取机制；
- 删除/启停/候选操作的统一事务与幂等语义。

验收：CLI 行为保持兼容；两个进程并发读写压力测试无数据损坏；任务不会重复领取；scheduler 关闭时网页任务保持 queued。

### 阶段 2：鉴权核心

交付：

- 管理员、Session、登录限流和审计 migrations；
- `auth admin create/reset-password/disable` 与 `auth sessions revoke-all`；
- Argon2id 密码服务；
- Session Cookie、过期、续期和撤销；
- Auth/CSRF/Origin/security-header middleware。

验收：无管理员时 fail closed；未登录无法访问任何管理数据；Cookie 属性完整；密码重置立即让旧 Session 失效；CSRF/Origin/过期 Session 测试全部通过。

### 阶段 3：只读网页

交付：

- 登录/退出；
- Dashboard；
- Anime 列表与详情；
- Candidate 列表与评分解释；
- Job、Provider、通知和同步状态；
- 分页、错误页和模板组件。

验收：页面默认转义恶意标题；未授权 HTML 重定向登录、API 返回 401；页面不泄漏数据库路径和 Secret；2 vCPU/1.7 GiB 环境空闲占用可接受。

### 阶段 4：追番管理与安全添加向导

交付：

- Bangumi 两步解析/确认 draft；
- 添加、启用、禁用；
- 输入标题 + nonce + optimistic check 的永久删除；
- 检查/同步任务入队；
- 所有操作审计。

验收：标题与显式 Bangumi ID 明显不符时不能静默添加；重复提交不创建重复 Anime/Job；删除与 scheduler 并发时不产生新通知。

### 阶段 5：候选与 UP 管理

交付：

- Candidate accept/reject；
- UP trust/block；
- pending notification 和失败重试状态；
- 操作后的 PRG 跳转和一次性提示。

验收：重复接受仍只有一条通知；同 MID 共识规则不变；被 block 的 UP 无法因网页操作绕过领域规则；全部写操作需要 CSRF 与审计。

### 阶段 6：生产部署

交付：

- `anipulse-web.service`；
- Caddy/Nginx 示例；
- admin bootstrap、Secret 生成、备份、升级、回滚教程；
- Tailscale 私网部署选项；
- 日志轮转与审计保留策略。

验收：8080 不对公网开放；HTTP 自动跳 HTTPS；web 服务没有飞书 Secret；关闭 web 不影响 scheduler；备份恢复演练成功。

### 阶段 7：安全与发布验收

交付：

- 完整单元、Router 集成、数据库并发和浏览器 E2E 测试；
- 依赖审计、静态检查和 release 构建；
- 安全 header、Cookie、CSRF、Session、限流和 XSS 回归套件；
- 文档和版本化 migration review。

验收：`cargo fmt --check`、`cargo test`、`cargo clippy -- -D warnings` 全通过；所有高影响路由都有未登录、无 CSRF、重复提交和成功路径测试；真实 HTTPS 环境完成登录、添加、检查、确认、删除和退出 smoke test。

## 14. 测试矩阵

### 14.1 单元测试

- Argon2 hash/verify、错误密码和 hash 参数升级；
- Session token entropy/编码/HMAC、idle/absolute/renewal；
- CSRF HMAC、scope、过期和常量时间比较；
- 用户名规范化、密码长度、配置边界；
- Bangumi draft 归属和过期；
- 删除确认标题、nonce 和 optimistic version；
- 审计 metadata allowlist。

### 14.2 Router 集成测试

- 所有保护路由未登录时的行为；
- 登录成功/失败/限流/账号禁用；
- `Set-Cookie` 完整属性；
- GET 无副作用；
- 写操作缺失/错误 CSRF、Origin、Content-Type；
- XSS payload 在标题、UP 名和错误文本中被转义；
- body size、超时、404/405 和内部错误不泄漏细节；
- 伪造 Forwarded header 不影响来源识别。

### 14.3 数据库与并发测试

- Web 与 scheduler 同时读写；
- job 原子领取、崩溃恢复和去重；
- Candidate accept 与通知幂等；
- 删除时所有 FK 级联和审计一致；
- WAL checkpoint、备份、migration 和回滚；
- `SQLITE_BUSY` 的受控重试与用户提示。

### 14.4 端到端测试

- 首次 admin bootstrap；
- HTTPS 登录/退出和 Session 续期；
- 两步添加追番；
- 立即检查任务从 queued 到 complete；
- 候选 accept 后 scheduler 发送通知；
- 禁用并永久删除；
- 改密后旧浏览器 Session 失效；
- web 重启不影响 scheduler，scheduler 重启不丢任务。

## 15. 资源预算

目标服务器为 2 vCPU、约 1.7 GiB RAM：

- 服务端渲染和 SQLite 日常请求负载很低；
- Web 连接池限制为 4 左右，Scheduler 继续低并发；
- Argon2 使用 `spawn_blocking` 且并发 1，参数在服务器实测后固定；
- 默认不启用 WebSocket、内存 Session store、大型前端构建或图片代理；
- 静态资源启用压缩和缓存，鉴权 HTML 明确 `no-store`；
- systemd 设置合理的 `MemoryMax`，但要高于单次 Argon2 内存成本和 Rust 进程正常峰值。

## 16. 备份、升级与回滚

每次 schema 变更前：

1. 停止 web 和 scheduler；
2. 使用 SQLite online backup 或停止后的完整文件备份；
3. 记录当前二进制 hash 和 migration 版本；
4. 运行 `database migrate`；
5. 先启动 scheduler 并检查，再启动 web；
6. 执行登录、列表和任务 smoke test。

新增独立表的早期阶段可通过关闭 web 并回滚二进制恢复；一旦 migration 改写现有业务表，只允许通过明确的 down migration 或数据库备份恢复。Session 和审计不是业务真相，恢复旧备份后允许全部 Session 失效，但 Anime/Episode/Candidate/Notification 必须保持一致。

## 17. V1 完成定义

- 网页默认只监听 loopback，并通过 HTTPS 访问；
- 没有默认账号、默认密码或匿名管理路由；
- 密码使用 Argon2id PHC hash，Session 是服务端不透明随机 Token；
- Cookie、CSRF、Origin、限流、安全头和审计全部生效；
- 网页服务不持有飞书 Secret，不直接请求外部 Provider；
- 添加向导必须确认 Bangumi 匹配；
- 删除需要禁用、输入标题、nonce 和服务端重新校验；
- CLI 全部保留，密码重置和灾难恢复不依赖网页；
- 网页故障不影响原有 scheduler；
- 所有阶段验收和生产 HTTPS smoke test 通过。

## 18. 后续增强（不属于 V1）

- WebAuthn/Passkey；
- TOTP 二次验证和恢复码；
- 接入现有 OIDC 身份提供方；
- 多管理员与只读角色；
- SSE 实时任务进度；
- 响应式移动页面/PWA；
- API Token，但必须独立 scope、过期、撤销和审计，不能复用浏览器 Session。

## 19. 参考资料

- [Axum 官方 crate 文档](https://docs.rs/axum/latest/axum/)
- [RustCrypto Argon2 官方 crate 文档](https://docs.rs/argon2/latest/argon2/)
- [OWASP Password Storage Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html)
- [OWASP Authentication Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Authentication_Cheat_Sheet.html)
- [OWASP Session Management Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Session_Management_Cheat_Sheet.html)
- [OWASP CSRF Prevention Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Cross-Site_Request_Forgery_Prevention_Cheat_Sheet.html)
- [OWASP HTTP Headers Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/HTTP_Headers_Cheat_Sheet.html)
- [MDN Secure cookie configuration](https://developer.mozilla.org/en-US/docs/Web/Security/Practical_implementation_guides/Cookies)
