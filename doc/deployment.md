# AniPulse 飞书私聊机器人与 Linux 部署教程

本文部署的是飞书“企业自建应用机器人”。AniPulse 使用 App ID 与 App Secret 获取应用访问凭证，然后直接向你的飞书账号发送消息卡片，不需要建立群聊。

整个集成只有服务器主动访问飞书的出站 HTTPS 请求，不需要事件订阅、回调域名、公网入站端口或付费功能。

## 1. 创建飞书自建应用

1. 登录[飞书开放平台](https://open.feishu.cn/)，进入“开发者后台”。
2. 选择“创建企业自建应用”，名称可填写 `AniPulse`。
3. 在“凭证与基础信息”中复制 `App ID` 和 `App Secret`。App ID 通常以 `cli_` 开头。
4. 进入“应用能力”→“添加应用能力”，添加“机器人”。
5. 进入“开发配置”→“权限管理”，申请应用身份权限“以应用的身份发消息”，权限标识为 `im:message:send_as_bot`。
6. 进入“版本管理与发布”，创建版本。把“可用范围”设置为只包含你自己，然后发布；如果需要管理员审核，由当前飞书组织管理员批准。

飞书官方的对应流程可参考[机器人应用配置说明](https://open.feishu.cn/document/develop-an-echo-bot/faq)。本项目只主动发通知，不读取你发送给机器人的内容，因此不用申请读取单聊消息权限，也不用配置事件订阅。

如果你的个人飞书账号还没有加入任何组织，可以先创建一个免费的飞书组织，并让自己成为管理员。创建自建应用和本项目所需的消息接口不要求购买席位。

## 2. 确定私聊收件人

最简单的方式是使用你在当前飞书组织通讯录中的邮箱：

```text
FEISHU_RECEIVE_ID_TYPE=email
FEISHU_RECEIVE_ID=you@example.com
```

这里应填写组织通讯录中记录的邮箱，不一定等同于你登录飞书时使用的手机号或第三方账号。

如果账号没有邮箱，也可以使用已经获得的用户 ID：

```text
FEISHU_RECEIVE_ID_TYPE=open_id
FEISHU_RECEIVE_ID=ou_xxxxxxxxxxxxxxxxxxxxxxxx
```

程序还支持 `user_id` 和 `union_id`。收件人必须处于该应用的可用范围内。具体参数格式见飞书官方的[发送消息接口](https://open.feishu.cn/document/server-docs/im-v1/message/create)。

## 3. 准备 Linux 服务器

以下命令以 Ubuntu/Debian 为例：

```bash
sudo apt update
sudo apt install -y build-essential cmake pkg-config curl git ca-certificates
```

使用服务器上的普通登录用户安装 Rust：

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
rustup update stable
```

将源码通过你的 Git 仓库或 `scp` 上传到服务器，然后进入源码目录：

```bash
cd /path/to/anipulse
cargo test --locked
cargo build --locked --release
```

## 4. 安装程序和专用用户

创建不可登录的系统用户：

```bash
sudo useradd --system --home /var/lib/anipulse --shell /usr/sbin/nologin anipulse
sudo install -d -o anipulse -g anipulse -m 0750 /var/lib/anipulse
```

安装二进制、配置和 systemd unit：

```bash
sudo install -m 0755 target/release/anipulse /usr/local/bin/anipulse
sudo install -d -m 0755 /etc/anipulse
sudo install -m 0644 config.example.toml /etc/anipulse/config.toml
sudo install -m 0644 deploy/anipulse.service /etc/systemd/system/anipulse.service
```

如果 `anipulse` 用户已经存在，跳过 `useradd` 即可。

## 5. 检查生产配置

编辑配置：

```bash
sudoedit /etc/anipulse/config.toml
```

至少确认以下内容：

```toml
[database]
path = "/var/lib/anipulse/anipulse.db"

[notification]
provider = "feishu"
channel = "feishu-private-anime"
notify_pending = false
request_timeout_secs = 15
```

`provider = "feishu"` 和 `provider = "feishu_app"` 都代表应用机器人私聊。`channel` 是 AniPulse 用于通知幂等的稳定标识，部署后不要随意修改，否则同一 Episode 可能因新 channel 名称产生另一条通知记录。

## 6. 保存飞书应用凭证

使用 `sudoedit` 创建环境文件：

```bash
sudoedit /etc/anipulse/anipulse.env
```

使用邮箱作为收件人的示例：

```text
FEISHU_APP_ID=cli_xxxxxxxxxxxxxxxxx
FEISHU_APP_SECRET=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
FEISHU_RECEIVE_ID_TYPE=email
FEISHU_RECEIVE_ID=you@example.com
RUST_LOG=info
```

限制文件权限：

```bash
sudo chown root:root /etc/anipulse/anipulse.env
sudo chmod 600 /etc/anipulse/anipulse.env
```

不要在环境文件中写 `export`，也不要把 App Secret 放进 TOML、Git、命令行参数或日志。AniPulse 的网络错误信息经过脱敏，不会打印 App Secret、访问令牌或完整请求。

## 7. 发送私聊测试卡片

下面的命令由 root 读取环境文件，但最终以 `anipulse` 用户运行程序，因此不会产生 root 所有的数据库：

```bash
sudo sh -c 'set -a; . /etc/anipulse/anipulse.env; exec runuser -u anipulse --preserve-environment -- /usr/local/bin/anipulse --config /etc/anipulse/config.toml notification test'
```

成功时，你和 `AniPulse` 应用机器人的单聊会话会收到“🧪 AniPulse 飞书通知测试”蓝色卡片，命令退出码为 `0`。

测试成功意味着以下环节都已经通过：

- App ID 与 App Secret 能获取 `tenant_access_token`；
- 应用已经发布并获得发消息权限；
- 你的账号位于应用可用范围内；
- 收件人类型与收件人值正确；
- 服务器能访问飞书开放平台。

## 8. 添加正在追的番剧

管理类 CLI 不需要读取飞书 Secret。示例：

```bash
sudo -u anipulse /usr/local/bin/anipulse \
  --config /etc/anipulse/config.toml \
  anime add \
  --title "Silent Witch" \
  --alias "沉默魔女" \
  --alias "サイレント・ウィッチ" \
  --next-episode 8 \
  --weekday friday \
  --time 23:00 \
  --timezone Asia/Shanghai \
  --duration-min 20m \
  --duration-max 28m
```

检查结果：

```bash
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml anime list
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml anime show 1
```

## 9. 启动 systemd 服务

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now anipulse
sudo systemctl status anipulse --no-pager
```

实时查看日志：

```bash
sudo journalctl -u anipulse -f
```

服务使用专用用户运行；`ProtectSystem=strict` 使系统目录只读，只允许写入 `/var/lib/anipulse`。它只需要通过 443 端口访问 Bilibili 和 `open.feishu.cn`，不需要在防火墙或路由器上开放入站端口。

## 10. 日常操作

查看待确认候选及解释：

```bash
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml candidate list --state pending --explain
```

人工确认或拒绝：

```bash
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml candidate accept BVxxxxxxxxxx
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml candidate reject BVxxxxxxxxxx
```

`candidate accept` 会创建 pending notification；常驻服务会在下一次调度循环私聊发送飞书卡片。通知失败会保留 pending 状态并指数退避重试，不会丢失 Episode，也不会因为一次超时生成重复通知。

## 11. 升级

在源码目录拉取新版本并验证：

```bash
git pull --ff-only
cargo test --locked
cargo build --locked --release
```

替换程序：

```bash
sudo systemctl stop anipulse
sudo install -m 0755 target/release/anipulse /usr/local/bin/anipulse
sudo systemctl start anipulse
sudo systemctl status anipulse --no-pager
```

SQLite migration 会在启动时自动执行。

## 12. 备份与恢复

最稳妥的离线备份方式：

```bash
sudo systemctl stop anipulse
sudo cp --preserve=mode,ownership /var/lib/anipulse/anipulse.db /var/lib/anipulse/anipulse.db.backup
sudo systemctl start anipulse
```

恢复前先停止服务，把备份复制回 `anipulse.db`，确认所有者仍为 `anipulse:anipulse`，再启动服务。

## 13. 常见问题

### 飞书没有收到测试卡片

先直接运行第 7 节的测试命令，再查看退出信息：

- `Feishu authentication failed`：核对 App ID 与 App Secret；修改 Secret 后重新写入环境文件。
- `Feishu message failed` 且提示权限不足：确认已经申请 `im:message:send_as_bot`，并在申请权限后重新发布应用。
- 提示机器人无法访问用户：确认应用可用范围包含你，并重新发布应用。
- 提示收件人无效：确认邮箱来自当前组织通讯录；或者改用 `open_id` 与 `ou_...`。
- HTTP timeout/connect：检查服务器能否通过 443 端口访问 `open.feishu.cn`。

程序会缓存飞书访问令牌，并在过期前自动刷新，不需要把 `tenant_access_token` 写入环境文件。

### 通知一直是 pending

发送失败时 AniPulse 会记录简化错误、保留 pending，并按指数退避再次发送。修复飞书配置后无需修改数据库，等待重试或重启服务即可。

### Bilibili 返回 412/429

这是 Provider 风控或限流。AniPulse 会持久化全局 backoff，期间不会持续高频请求。不要通过代理池、Cookie 池或高频重启绕过。

## 14. 可选：继续使用飞书群 Webhook

如果以后希望改发到群聊，把配置改为：

```toml
[notification]
provider = "feishu_webhook"
channel = "feishu-group-anime"
```

然后在环境文件中提供：

```text
FEISHU_WEBHOOK_URL=https://open.feishu.cn/open-apis/bot/v2/hook/你的-token
FEISHU_BOT_SECRET=飞书安全设置中的签名密钥
```

群 Webhook 和应用机器人私聊是两套独立凭证，不能混用。
