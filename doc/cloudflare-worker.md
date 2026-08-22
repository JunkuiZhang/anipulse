# 使用 Cloudflare Worker 转发 Bangumi 数据

这个方案不替换 AniPulse 的排期来源：排期仍然来自 `bangumi-data`，章节日期和封面仍然来自 Bangumi。变化只是让阿里云服务器访问你自己的 Cloudflare 子域名，再由 Worker 请求境外上游。

新版 AniPulse 还会从 `bangumi-data` 的 `sites[].begin/broadcast` 选择 U-NEXT、d Anime、ABEMA 或动画疯等网络时段。这些内容已经包含在 `/data.json` 里；ECS 和 Worker 都**不会访问这些平台的网站**，也不需要为它们新增代理路由。

```text
阿里云 AniPulse
  └── https://bgm-proxy.example.com
        ├── /data.json                  → unpkg 上的 bangumi-data
        ├── /bangumi/v0/episodes        → api.bgm.tv 章节 API
        └── /bangumi/v0/subjects/...    → api.bgm.tv，并在 Worker 内跟随封面重定向
```

Worker 位于 [`deploy/cloudflare-worker`](../deploy/cloudflare-worker)。它不是通用反向代理：只接受 AniPulse 当前使用的三个固定路由、严格校验查询参数，并只允许配置的服务器出口 IP。

## 1. 前提

- 域名的 DNS Zone 已托管在 Cloudflare；
- 准备一个没有被其他服务占用的子域名，例如 `bgm-proxy.example.com`；
- 阿里云 ECS 有稳定的公网出口 IPv4 或 IPv6；
- 如果选择命令行部署，部署电脑需有 Node.js 20 或更高版本。完全通过 Cloudflare 网页部署时不需要 Node.js，也不需要在 ECS 安装任何 Cloudflare 工具。

可以从阿里云控制台确认实例公网 IP。若实例通过 NAT 网关出站，应填写 NAT 网关的出口 IP，而不是实例内网地址。

普通 Cloudflare 全球网络不等于 [Cloudflare 中国网络](https://developers.cloudflare.com/china-network/)。个人免费方案通常可以从大陆访问，但跨境链路没有可用性保证；真正的中国网络是 Enterprise 的独立订阅，并要求 ICP。AniPulse 的数据源故障飞书告警仍然应当保留。

Workers Free 当前包含每天 100,000 次请求，对个人 AniPulse 足够使用；具体额度以 Cloudflare 的 [Workers Limits](https://developers.cloudflare.com/workers/platform/limits/) 为准。

## 2. 完全通过 Cloudflare Dashboard 部署

下面的流程不需要在本地安装 Wrangler，适合直接在 Cloudflare 网页完成。

### 2.1 创建 Worker

1. 登录 [Cloudflare Dashboard](https://dash.cloudflare.com/)；
2. 打开 **Workers & Pages**；
3. 选择 **Create application → Create Worker**。新版界面也可能显示为 **Start with Hello World**；
4. 名称填写 `anipulse-bangumi-proxy`，然后点击 **Deploy**；
5. 首次部署完成后，点击 **Edit code**；
6. 删除编辑器里的示例代码，将 [`deploy/cloudflare-worker/src/index.js`](../deploy/cloudflare-worker/src/index.js) 的完整内容粘贴进去；
7. 点击右上角 **Deploy**。

这里必须粘贴整个文件，不能只复制其中的 `fetch` 函数。Worker 依赖同一文件里的路由白名单、缓存和错误处理代码。

### 2.2 设置 Variables and Secrets

进入这个 Worker 的 **Settings → Variables and Secrets**，依次添加：

| 名称 | 类型 | 是否必需 | 值 |
|---|---|---:|---|
| `ALLOWED_CLIENT_IPS` | Secret | 是 | ECS 或 NAT 网关的实际公网出口 IP；多个地址用英文逗号分隔 |
| `UPSTREAM_USER_AGENT` | Secret | 是 | 例如 `你的-Bangumi-用户名/AniPulse/0.1 (personal self-hosted via Cloudflare Worker)` |
| `CACHE_VERSION` | Text | 否 | 初次可填 `v1`，需要绕过旧缓存时改成 `v2` |

保存后按界面提示重新部署。不要把实际公网 IP、User-Agent 身份或 Secret 写入 Git、截图或公开文档。

如果 ECS 通过 NAT 网关出站，`ALLOWED_CLIENT_IPS` 必须填 NAT 网关的出口 IP。可在 ECS 上用 Cloudflare 的 trace 页面确认 Worker 实际看到的地址：

```bash
curl -fsS https://www.cloudflare.com/cdn-cgi/trace | grep '^ip='
```

Worker 未配置 `ALLOWED_CLIENT_IPS` 时会返回 HTTP 503；访问地址不在名单中时返回 HTTP 403。这是刻意的 fail-closed 设计。

### 2.3 绑定自定义域名

1. 打开 Worker 的 **Settings → Domains & Routes**；
2. 选择 **Add → Custom Domain**；
3. 输入一个未被占用的子域名，例如 `bgm-proxy.example.com`；
4. 确认添加，等待 Cloudflare 自动创建 DNS 记录和 TLS 证书。

不要再手工给这个子域名添加指向 ECS 的 A/AAAA/CNAME 记录；它的源站就是 Worker。如果已经存在同名 DNS 记录，先确认没有其他服务使用，再删除冲突记录后重试。

网页编辑器的 Preview 请求通常来自 Cloudflare 或你的本机，而不是 ECS，因此配置 IP 白名单后 Preview 返回 403 是正常的。最终验证应从 ECS 执行本文第 6 节的命令。

Cloudflare 官方参考：[Dashboard 创建 Worker](https://developers.cloudflare.com/workers/get-started/dashboard/)、[Secrets](https://developers.cloudflare.com/workers/configuration/secrets/) 和 [Custom Domains](https://developers.cloudflare.com/workers/configuration/routing/custom-domains/)。

## 3. 本地测试 Worker（可选）

进入 Worker 目录：

```bash
cd deploy/cloudflare-worker
cp .dev.vars.example .dev.vars
```

将 `.dev.vars` 中的 `UPSTREAM_USER_AGENT` 改成自己的稳定标识。`.dev.vars` 已被 Git 忽略，不要提交实际配置。

运行不需要网络和第三方测试库的单元测试：

```bash
npm test
```

需要本地启动时执行：

```bash
npm run dev
```

本地请求来自 `127.0.0.1` 或 `::1`，示例 allowlist 已包含它们。

## 4. 使用 Wrangler 部署（可选）

```bash
cd deploy/cloudflare-worker
npx wrangler@latest login
npx wrangler@latest deploy
```

`wrangler.jsonc` 默认关闭 `workers.dev` 公网地址。首次部署后 Worker 还没有对外入口，这是刻意的；下一步使用自己的子域名。

设置仅允许访问 Worker 的阿里云出口 IP。命令会提示输入值，多个地址用英文逗号分隔：

```bash
npx wrangler@latest secret put ALLOWED_CLIENT_IPS
```

输入示例：

```text
203.0.113.10,2001:db8::10
```

`203.0.113.10` 和 `2001:db8::10` 都是文档保留地址，只用于演示；部署时应在 Secret 中填写自己的实际出口地址，不要提交到仓库。

再设置 Bangumi 要求的可识别 User-Agent：

```bash
npx wrangler@latest secret put UPSTREAM_USER_AGENT
```

输入示例：

```text
你的-Bangumi-用户名/AniPulse/0.1 (personal self-hosted via Cloudflare Worker)
```

Worker 没有配置 `ALLOWED_CLIENT_IPS` 时会 fail closed，所有请求返回 HTTP 503；IP 不在名单中时返回 HTTP 403。

## 5. 为 Wrangler 部署绑定自定义域名

进入 Cloudflare Dashboard：

1. 打开 **Workers & Pages**；
2. 选择 `anipulse-bangumi-proxy`；
3. 打开 **Settings → Domains & Routes**；
4. 添加 **Custom Domain**；
5. 输入 `bgm-proxy.example.com`。

Cloudflare 会为该子域名创建 DNS 记录和 TLS 证书。这个子域名的源站就是 Worker，不要再将它解析到阿里云或其他服务器。

Cloudflare 官方也建议生产 Worker 使用 [Custom Domain 或 Route](https://developers.cloudflare.com/workers/configuration/routing/)，而不是把 `workers.dev` 当作正式入口。

## 6. 从阿里云验证

以下命令必须在 ECS 上执行，因为本机 IP 默认不在 allowlist 中。

检查 Worker：

```bash
curl -fsS https://bgm-proxy.example.com/healthz
```

预期：

```json
{"ok":true,"service":"anipulse-bangumi-proxy"}
```

检查 `bangumi-data`，避免把完整 JSON 打到终端：

```bash
curl -fsS -D /tmp/anipulse-worker-data.headers \
  -o /tmp/anipulse-worker-data.json \
  https://bgm-proxy.example.com/data.json
wc -c /tmp/anipulse-worker-data.json
grep -i x-anipulse-proxy-cache /tmp/anipulse-worker-data.headers
```

检查章节 API：

```bash
curl -fsS \
  'https://bgm-proxy.example.com/bangumi/v0/episodes?subject_id=622206&type=0&limit=1&offset=8'
```

排期校准会分别读取目标集和本季起始集，因此也要确认 `offset=0` 可用。Worker 允许的是经过校验的任意非负 `offset`，不是只放行某一个集数：

```bash
curl -fsS \
  'https://bgm-proxy.example.com/bangumi/v0/episodes?subject_id=622206&type=0&limit=1&offset=0'
```

检查封面。Worker 必须直接返回 `image/*`，而不是将客户端重定向到 `lain.bgm.tv`：

```bash
curl -fsS -D /tmp/anipulse-worker-cover.headers \
  -o /tmp/anipulse-worker-cover \
  'https://bgm-proxy.example.com/bangumi/v0/subjects/622206/image?type=medium'
file /tmp/anipulse-worker-cover
grep -iE 'content-type|x-anipulse-proxy-cache' /tmp/anipulse-worker-cover.headers
```

第一次通常显示 `X-AniPulse-Proxy-Cache: MISS`，同一 Cloudflare 节点的后续请求应显示 `HIT`。Cloudflare Cache API 是按边缘节点缓存，因此换网络或换地区后首次请求仍可能是 `MISS`。

## 7. 修改 AniPulse 配置

先备份配置：

```bash
sudo cp -a /etc/anipulse/config.toml /etc/anipulse/config.toml.before-worker
```

编辑 `/etc/anipulse/config.toml`：

```toml
[schedule]
bangumi_data_url = "https://bgm-proxy.example.com/data.json"
bangumi_api_base_url = "https://bgm-proxy.example.com/bangumi"
preferred_site = "bilibili"
stream_site_priority = ["unext", "danime", "abema", "gamer", "gamer_hk"]
max_stream_offset_days = 14
max_catalog_offset_days = 1
```

`bangumi_api_base_url` 不要写 `/v0`；AniPulse 会自己追加 `/v0/episodes` 和封面路径。

`stream_site_priority` 是可信来源集合兼决胜顺序，不再表示“找到第一个就停止”。AniPulse 会在本地比较 `/data.json` 中的全部候选并选择最早的独立平台共识；Worker 不需要新增任何上游网站或路由。

调度器和网页封面服务都会读取该配置，因此两个服务都要重启：

```bash
sudo systemctl restart anipulse.service anipulse-web.service
sudo systemctl --no-pager --full status anipulse.service anipulse-web.service
```

手工同步一个自动排期条目：

```bash
sudo -u anipulse /usr/local/bin/anipulse \
  --config /etc/anipulse/config.toml \
  anime sync 1
```

然后刷新网页封面并检查日志：

```bash
sudo journalctl -u anipulse.service -u anipulse-web.service -n 100 --no-pager
```

如果封面之前一直是占位图，成功请求后会写入 `/var/lib/anipulse/covers`。浏览器随后继续通过 AniPulse 自己的 `/covers/{subject_id}` 读取服务器本地缓存，不会直接访问 Worker。

## 8. 缓存策略

Worker 使用以下边缘缓存时间：

| 内容 | Cloudflare 边缘缓存 | 返回给客户端的缓存 |
|---|---:|---:|
| `bangumi-data` | 6 小时 | 5 分钟 |
| 章节日期 | 15 分钟 | 1 分钟 |
| 封面 | 30 天 | 1 天 |

封面下载到 AniPulse 服务器后，还有现有的 7 天服务器缓存和浏览器条件缓存。删除番剧时，AniPulse 会按现有清理逻辑删除不再被数据库引用的本地封面；Cloudflare 上的匿名公共封面缓存会在 TTL 到期后自然淘汰。

需要立即绕过旧 Worker 缓存时，在 Worker 的 Variables and Secrets 中添加或修改普通变量 `CACHE_VERSION`，例如从 `v1` 改为 `v2`。新版本会使用新的 cache key，不需要修改 AniPulse URL。

### 更新 Worker

本次“电视/网络排期校准”没有增加 Worker 路由：如果当前 Worker 已经来自本仓库，并且上面的 `offset=0` 与目标集请求都成功，只升级 AniPulse 二进制即可，不必重新部署 Worker。

以后仓库中的 [`deploy/cloudflare-worker/src/index.js`](../deploy/cloudflare-worker/src/index.js) 有更新时，可完全通过网页升级：

1. 打开 Cloudflare Dashboard → **Workers & Pages** → `anipulse-bangumi-proxy`；
2. 先在 **Settings → Variables and Secrets** 确认 `ALLOWED_CLIENT_IPS` 和 `UPSTREAM_USER_AGENT` 仍存在；不要把它们复制进源代码；
3. 点击 **Edit code**，用仓库中 `src/index.js` 的完整内容替换旧代码；
4. 点击 **Deploy**；域名、Secret 和现有变量会保留；
5. 如果路由/解析逻辑改变或怀疑命中了旧缓存，把 `CACHE_VERSION` 从例如 `v1` 改成 `v2`，然后再次部署；
6. 回到 ECS，依次验证 `/healthz`、`/data.json`、两个章节 offset 和封面，再重启 AniPulse 两个服务。

更新前可把网页编辑器中的旧代码保存到本地作为回滚副本。若新版本异常，重新粘贴旧代码并 Deploy；不要在文档、Git、截图或聊天记录中暴露实际出口 IP 与 Secret。

## 9. 故障排查

### HTTP 403

阿里云实际出口 IP 不在 `ALLOWED_CLIENT_IPS`。通过阿里云控制台或 NAT 网关配置确认出口地址，然后重新执行：

```bash
npx wrangler@latest secret put ALLOWED_CLIENT_IPS
```

### HTTP 503

Worker 没有配置 `ALLOWED_CLIENT_IPS`，或者配置值为空。

### HTTP 404

路径或查询参数不属于 AniPulse allowlist。章节 API 只允许 `subject_id`、`type=0`、`limit=1`、`offset`；封面只允许 `type=medium`。

### HTTP 502/504

上游返回错误、内容类型异常、内容超过限制，或者 Worker 到上游超时。查看 Cloudflare Worker Logs，同时保留 AniPulse 飞书数据源告警。Worker 不会把上游错误正文或内部重定向地址直接暴露给客户端。

### 回滚

恢复配置并重启两个服务：

```bash
sudo cp -a /etc/anipulse/config.toml.before-worker /etc/anipulse/config.toml
sudo systemctl restart anipulse.service anipulse-web.service
```

回滚只改变网络入口，不修改 SQLite、追番、排期或本地封面缓存。
