# AniPulse 网页管理端部署教程

网页管理端是独立的 `anipulse web` 进程：它和监控调度器共享 SQLite，但只读取 `ANIPULSE_WEB_SECRET`，不读取飞书 App Secret。默认只监听 `127.0.0.1:8080`，必须由 Caddy 通过 HTTPS 暴露。

部署后的使用链路是：

```text
飞书私聊审核卡片 → HTTPS 网页 → 登录 → 选择候选 / 都不选 / 提交 B 站链接
                                      ↓
                                SQLite management_job
                                      ↓
                    anipulse run 请求 Bilibili / Bangumi / 飞书
```

飞书卡片里的按钮只打开页面，不会匿名修改数据库。所有网页写操作都要求 Session、同源检查和 CSRF；接受候选、全部拒绝和永久删除还要求一次性确认。

## 1. 准备域名和端口

准备一个域名，例如 `anime.example.com`，把 A/AAAA 记录指向服务器。安全组和系统防火墙只需要允许：

- SSH 管理端口；
- TCP 80（Caddy 申请证书和跳转 HTTPS）；
- TCP 443（网页 HTTPS）。

不要开放 8080。AniPulse 会拒绝非 loopback 的 `web.bind`，除非显式开启危险选项。

如果不想把登录页放到公网，可通过 Tailscale/WireGuard 私网域名提供 HTTPS；配置步骤相同，只需换成该私网 HTTPS 地址。

## 2. 安装新二进制和部署文件

先按 [`deployment.md`](deployment.md) 的构建章节得到能在 Ubuntu 22.04 运行的二进制。升级已有部署前先停服务并备份：

```bash
sudo systemctl stop anipulse-web.service anipulse.service 2>/dev/null || true
sudo cp --preserve=mode,ownership \
  /var/lib/anipulse/anipulse.db \
  /var/lib/anipulse/anipulse.db.before-web
```

安装新文件：

```bash
sudo install -m 0755 target/release/anipulse /usr/local/bin/anipulse
sudo install -m 0644 deploy/anipulse.service /etc/systemd/system/anipulse.service
sudo install -m 0644 deploy/anipulse-web.service /etc/systemd/system/anipulse-web.service
```

如果是在 Arch 上交叉构建，请把第一条命令中的源路径换成经过 `file` 和 `readelf` 检查的静态 musl 产物，不要使用 Arch 的 `target/release/anipulse`。

## 3. 配置网页

编辑 `/etc/anipulse/config.toml`，将域名替换为你的真实域名：

```toml
[web]
bind = "127.0.0.1:8080"
public_url = "https://anime.example.com"
cover_cache_dir = "covers"
timezone = "Asia/Shanghai"
trusted_proxy_cidrs = ["127.0.0.1/32", "::1/128"]
session_idle_secs = 7200
session_absolute_secs = 86400
session_renewal_secs = 1800
login_window_secs = 900
login_max_failures = 5
request_timeout_secs = 15
max_body_bytes = 65536
development_mode = false
dangerous_allow_public_bind = false
```

`public_url` 必须和浏览器实际访问的 origin 完全一致，包括非默认端口。生产环境必须是 HTTPS；不要为了省略反向代理而把 `development_mode` 或 `dangerous_allow_public_bind` 打开。

`timezone` 是网页统一使用的 IANA 时区。数据库仍以 UTC 保存时间，网页展示时才转换，因此修改它不需要迁移数据库。中国大陆通常使用 `Asia/Shanghai`；例如数据库中的 `2026-08-21 13:25:20 UTC` 会显示为 `2026-08-21 21:25:20`，页面不会在每个时间后面重复显示时区名称。

`cover_cache_dir = "covers"` 会把 Bangumi 封面保存到 `/var/lib/anipulse/covers`（相对路径以 systemd 的 `WorkingDirectory` 为基准）。第一次显示某张封面时由网页进程下载，服务器缓存 7 天；图片 API 失败时会并行尝试从 Bangumi 官方条目页发现封面，上游临时不可用时则继续返回已经存在的旧图。浏览器收到 `Cache-Control: private, max-age=86400` 和 ETag，会缓存 1 天，过期后通常只向 AniPulse 做条件校验，不会再次下载完整图片。缓存单图上限为 5 MiB，只接受常见位图格式；下载失败占位图不缓存，刷新页面即可重试。

网页进程会在启动时和此后每小时对照数据库清理缓存；网页中永久删除追番后还会立即清理。只有当同一个 Bangumi subject ID 不再被任何追番引用时才删除对应文件，因此重复绑定不会误删共享封面。通过 CLI 删除的缓存最迟在一小时后清理，也可以重启 `anipulse-web.service` 立即触发。缓存目录中 AniPulse 不认识的其他扩展名文件不会被删除。

若 Caddy 与 AniPulse 在同一台服务器，默认可信代理范围无需修改。不要写 `0.0.0.0/0`；只有 TCP peer 属于这里的 CIDR 时，登录限流才会读取 `X-Forwarded-For`。

## 4. 生成网页 Secret

创建独立的网页环境文件：

```bash
sudo sh -c 'umask 077; printf "ANIPULSE_WEB_SECRET=" > /etc/anipulse/anipulse-web.env; openssl rand -base64 48 >> /etc/anipulse/anipulse-web.env; printf "RUST_LOG=info\n" >> /etc/anipulse/anipulse-web.env'
sudo chown root:root /etc/anipulse/anipulse-web.env
sudo chmod 600 /etc/anipulse/anipulse-web.env
```

这个 Secret 用 HKDF 派生 Session、CSRF、一次性操作和审计哈希密钥。不要和 `FEISHU_APP_SECRET` 混用，不要放进 TOML 或 Git。Secret 丢失或更换后，已有网页 Session 和未完成的一次性确认会失效，但追番数据不受影响。

`anipulse-web.service` 只加载 `anipulse-web.env`；原来的 `anipulse.service` 继续只加载包含飞书凭证的 `anipulse.env`。

## 5. 迁移数据库并创建管理员

两个服务都保持停止时执行 migration：

```bash
sudo -u anipulse /usr/local/bin/anipulse \
  --config /etc/anipulse/config.toml \
  database migrate
```

然后通过 TTY 创建唯一的 owner 管理员：

```bash
sudo -u anipulse /usr/local/bin/anipulse \
  --config /etc/anipulse/config.toml \
  auth admin create --username admin
```

程序会隐藏密码输入并要求重复确认；密码至少 12 个字符。没有默认账号、默认密码，也不能在登录页注册。

恢复命令如下：

```bash
# 重置密码，同时撤销全部旧 Session
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml \
  auth admin reset-password --username admin

# 只撤销全部登录
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml \
  auth sessions revoke-all --username admin

# 紧急禁用账号并撤销全部登录
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml \
  auth admin disable --username admin
```

## 6. 配置 Caddy HTTPS

按 [Caddy 官方安装说明](https://caddyserver.com/docs/install)安装 Caddy，然后复制示例：

```bash
sudo cp deploy/Caddyfile.example /etc/caddy/Caddyfile
sudoedit /etc/caddy/Caddyfile
```

把第一行的 `anime.example.com` 换成实际域名。示例会限制请求体、设置 HSTS、覆盖代理来源信息并反向代理到 `127.0.0.1:8080`。验证并加载：

```bash
sudo caddy validate --config /etc/caddy/Caddyfile
sudo systemctl enable --now caddy
sudo systemctl reload caddy
sudo systemctl status caddy --no-pager
```

Caddy 默认会为域名申请 HTTPS 证书，并在反向代理时安全地重建 `X-Forwarded-*` 头。若域名前还有 Cloudflare 等 CDN，不能直接沿用默认可信代理设置；先按 Caddy 官方 `trusted_proxies` 文档配置真实 CDN 网段，再把 AniPulse 的 `trusted_proxy_cidrs` 保持为本机 Caddy 地址。

## 7. 启动两个服务

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now anipulse.service anipulse-web.service
sudo systemctl status anipulse.service anipulse-web.service --no-pager
```

本机确认监听边界和健康检查：

```bash
ss -ltnp | grep -E ':443|127\.0\.0\.1:8080'
curl -i http://127.0.0.1:8080/healthz
curl -I https://anime.example.com/login
```

8080 应只出现在 `127.0.0.1`，`/healthz` 只返回固定的 `ok`。打开 `https://anime.example.com/login` 登录后，可看到 Dashboard、卡片式追番、候选、过滤与信任规则、后台任务、审计和状态页面。

网页空闲时不请求 Bilibili。点击“立即检查”“同步排期”“测试通知”或提交 B 站链接只会写入 `management_job`；`anipulse run` 在下一个 scheduler tick 领取任务。因此 scheduler 停止时任务会保持 `queued`，网页仍可查看和编辑本地数据。

添加追番时，“特殊集数映射”可以解决站内集数和 Bangumi 章节编号不同的问题。两个起点必须同时填写，例如“站内 EP12 ↔ Bangumi EP78”；之后站内 EP14 会使用 Bangumi EP80 校准日期，同时 Bilibili 搜索仍保持 EP14。已有追番可以在详情页修改映射，保存后会自动创建排期同步任务；清空两项即恢复普通的 EP1 ↔ EP1 规则。

### 追番编号、播完与归档

“我的追番”卡片左上角的 `#1`、`#2` 是当前追番列表中的显示序号，不是 SQLite 主键。删除或归档中间条目后，页面会自动连续编号；CLI、URL 和审计仍使用不会变化的数据库 ID。

番剧生命周期分为三步：

1. **监控中**：正常同步排期并检查新集；
2. **已播完 · 待看完**：自动排期能从 Bangumi 正篇章节总数确认最后一集时，会在最后一集发布后自动进入；也可以从详情页手工标记。此状态停止查找下一集，但保留待看视频；
3. **已归档**：在首页把最后一集以及此前所有待看集标记为“已观看”后自动进入；也可以从详情页手工二次确认归档。

AniPulse 从 Bangumi `/v0/episodes?type=0` 的分页总数读取正篇集数，不把 SP、OP、ED 等章节计入。如果配置了“站内 EP12 ↔ Bangumi EP78”映射，而 Bangumi 条目共有 8 个正篇章节，则页面显示“8 集 · 站内最终 EP19”，并以 EP19 判断最后一集。总集数请求失败时保留上次结果；从未成功取得总数时不会自动判定完结或归档。

“最后一集已经发布”本身仍不会自动归档，只会进入“已播完 · 待看完”。自动归档必须由“已观看”操作触发，并且本季不能还有其他待看集。误判播完时，可以在详情页指定下一集并恢复监控。

归档会清理 Episode、候选、通知、逐集视频来源、UP 信任和非运行中的后台任务，保留标题、别名、Bangumi 绑定、服务器本地封面、总集数、简介/观看记录和归档时间。归档收藏在“追番 → 归档收藏”中查看；永久删除收藏仍需标题确认。升级数据库不会自动归档任何现有条目，只有明确点击“已观看”或手工确认归档才会触发。

## 8. 开启飞书“不确定候选”提醒

确认 HTTPS 网页可以登录后，再修改通知配置：

```toml
[notification]
provider = "feishu"
channel = "feishu-private-anime"
notify_pending = true
request_timeout_secs = 15
review_grace_secs = 3600
```

重启 scheduler：

```bash
sudo systemctl restart anipulse.service
```

行为如下：

- 找到候选但可信 UP/独立共识不足：候选集合变化时发送一张橙色审核卡片；
- “泄露/偷跑/盗录”等高风险标题和低于粉丝硬门槛的 UP 会直接拒绝；低播放视频或详情元数据不完整时只进入人工审核，不能参与自动共识；
- 到预计更新时间后仍没有候选：等待 `review_grace_secs` 后发送一次审核卡片；
- Bilibili 超时、412/429、软风控或全局 backoff：不发送“没有候选”的误导提醒；
- 同一个候选集合只提醒一次，失败会指数退避重试；
- 审核提醒不会推进下一集，也不会替代绿色“已更新”通知。

在审核页可以选择一个候选、将页面当时展示的候选全部拒绝，或提交规范的 B 站 HTTPS `/video/BV...` 链接。候选会显示 UP 粉丝、播放量和评论数；链接由 scheduler 获取真实标题、UP、时长和信誉数据后保存为待确认候选，你还需要在页面上最终点一次确认。

候选列表中的“屏蔽此 UP”只对当前番剧生效：它会在一个事务中记录屏蔽状态，并拒绝该 UP 在此番剧下已有的全部待审核候选；以后检测到的候选也会自动排除。操作完成后页面会显示结果提示。“信任此 UP”同样只对当前番剧生效。

导航栏的“规则”页提供两类可编辑规则：

- 屏蔽词是全局规则，匹配候选的标题、简介或标签后硬拒绝；新建或修改后从下一次检查开始生效，当前仍在列表中的候选可以先手工拒绝或点“立即检查”重新评估；
- 信任 UP 按番剧生效，可以添加、改 UID/显示名称、换所属番剧或移除。候选页点“信任此 UP”创建的记录也会出现在这里；信任不绕过番名、集数、时长和屏蔽词规则。

候选确认和拒绝的网页路径同时绑定 Episode ID 与 BV 号。旧审核页即使仍开在浏览器里，也不能用上一集的同 BV 号确认当前集；一集确认后，同集未选择的候选立即变成“已过期”。

## 9. 升级、回滚和日志

以后升级网页 schema：

```bash
sudo systemctl stop anipulse-web.service anipulse.service
sudo cp --preserve=mode,ownership /var/lib/anipulse/anipulse.db \
  /var/lib/anipulse/anipulse.db.$(date +%Y%m%d-%H%M%S).backup
sudo install -m 0755 target/release/anipulse /usr/local/bin/anipulse
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml database migrate
sudo systemctl start anipulse.service anipulse-web.service
```

查看日志：

```bash
sudo journalctl -u anipulse.service -f
sudo journalctl -u anipulse-web.service -f
sudo tail -f /var/log/caddy/anipulse-access.log
```

网页日志不会记录表单正文、Cookie 或 Secret。数据库处于 WAL 模式，网页和 scheduler 可以并发使用；如果 scheduler 在执行后台任务时中断，运行超过 10 分钟的 `running` 任务会被重新入队。

回滚时先停止两个服务。如果只是关闭网页，可直接禁用 `anipulse-web.service`，原有 scheduler 和 CLI 不受影响。若需要回到不认识新 migration 的旧版本，恢复升级前的完整数据库备份，而不是手工删除表。

## 10. 常见问题

### 登录后反复回到登录页

检查浏览器访问地址是否与 `web.public_url` 完全相同，并确认是 HTTPS。查看响应中的 Cookie 是否具有 `Secure; HttpOnly; SameSite=Lax; Path=/`。修改 `ANIPULSE_WEB_SECRET` 会让旧 Cookie 立即失效，这是预期行为。

### POST 显示“请求来源校验失败”

通常是 `web.public_url` 域名或端口不匹配，或者反向代理改写了浏览器可见 origin。不要关闭 Origin/CSRF 校验；把 `public_url` 改成浏览器地址并重启 web。

### 添加追番一直显示“后台解析中”

自动排期解析由 scheduler 的任务执行。确认 `anipulse.service` 正常、有 Bangumi 网络访问，并在“任务”页查看错误。修复后重新走一次添加向导即可；15 分钟未确认的 draft 会过期。

### 封面一直显示“封面稍后重试”

先刷新页面；失败占位图不会被浏览器缓存。然后查看网页服务记录的两个官方来源错误：

```bash
sudo journalctl -u anipulse-web.service -n 100 --no-pager \
  | grep -E 'cover unavailable|cover refresh|622206'
```

把下面的 subject ID 和 User-Agent 换成你的实际值，并且必须以运行服务的 `anipulse` 用户测试：

```bash
sudo -u anipulse curl -I -L --max-time 20 \
  -A 'your-name/AniPulse/0.1 (personal self-hosted)' \
  'https://api.bgm.tv/v0/subjects/622206/image?type=medium'

sudo -u anipulse curl -I -L --max-time 20 \
  -A 'your-name/AniPulse/0.1 (personal self-hosted)' \
  'https://bgm.tv/subject/622206'

getent ahostsv4 api.bgm.tv bgm.tv lain.bgm.tv
sudo -u anipulse test -w /var/lib/anipulse/covers && echo 'cover cache writable'
```

第一条请求会重定向到 `lain.bgm.tv`，因此服务器需要能够通过 HTTPS 访问 `api.bgm.tv`、`bgm.tv` 和 `lain.bgm.tv`。如果 curl 超时或连接失败，问题在服务器的出站网络、DNS 或到 Bangumi 的链路；这与浏览器缓存无关。如果 curl 成功但网页仍失败，以 journal 中的具体错误为准，并检查缓存目录权限。成功一次后图片会写入本地缓存，之后即使上游暂时失败也会继续显示旧图。修复后可重启网页服务并刷新：

```bash
sudo systemctl restart anipulse-web.service
```

### 飞书卡片按钮打不开

检查 `web.public_url` 是否为手机/电脑可以访问的真实 HTTPS 地址。`localhost` 指的是打开飞书的设备自己，不能用于服务器部署。

### 页面显示 scheduler 已停止或任务一直 queued

```bash
sudo systemctl status anipulse.service --no-pager
sudo journalctl -u anipulse.service -n 100 --no-pager
```

网页服务不会替 scheduler 执行外部请求，这是刻意的 Secret 与故障隔离。

### 旧版本误把上一集视频确认成当前集并推进了一集

例如 EP8 视频被错误当作 EP9 推送，页面已经显示等待 EP10。先确认 Anime ID；下面以尼古喵喵 ID `1`、应恢复到 EP9 为例：

```bash
sudo systemctl stop anipulse.service
sudo cp --preserve=mode,ownership /var/lib/anipulse/anipulse.db \
  /var/lib/anipulse/anipulse.db.before-episode-repair

sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml anime show 1
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml anime disable 1

# 不带 --yes 会拒绝执行并显示将要从哪一集回退，可先用它复核。
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml \
  anime repair-episode 1 --episode 9
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml \
  anime repair-episode 1 --episode 9 --yes

sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml anime show 1
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml anime enable 1
sudo systemctl start anipulse.service
sudo journalctl -u anipulse.service -n 100 --no-pager
```

修复是一个 SQLite 事务：删除目标集的错误通知记录、使该集 pending/confirmed 候选过期、删除更后面的派生 Episode，并把目标集恢复为 `waiting` 且立即检查。命令要求番剧先处于禁用状态，并校验当前 Episode 在操作期间没有变化。已经发到飞书的错误卡片无法撤回。
