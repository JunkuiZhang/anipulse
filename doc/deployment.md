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

最简单的方式是使用你在当前飞书组织通讯录中的邮箱。按以下步骤查找：

1. 登录[飞书管理后台](https://www.feishu.cn/admin)。
2. 进入“组织架构”→“成员与部门”→“成员”。
3. 点击你自己的名字，打开成员详情。
4. 复制详情中的“邮箱”字段。

这里应填写成员资料中的邮箱，不一定等同于你登录飞书时使用的手机号、第三方账号或后来绑定的邮箱。将它写入 `/etc/anipulse/anipulse.env`：

```text
FEISHU_RECEIVE_ID_TYPE=email
FEISHU_RECEIVE_ID=you@example.com
```

如果成员资料中没有邮箱，可以在同一个成员详情页面找到并复制“用户 ID”：

```text
FEISHU_RECEIVE_ID_TYPE=user_id
FEISHU_RECEIVE_ID=成员详情中的用户ID
```

使用 `user_id` 时，在自建应用的“开发配置”→“权限管理”中开通“获取用户 user ID”（`contact:user.employee_id:readonly`），然后创建新版本并重新发布。飞书也支持通过邮箱或手机号调用[获取用户 ID 接口](https://open.feishu.cn/document/server-docs/contact-v3/user/batch_get_id)，但个人部署直接从管理后台复制更简单。

程序也支持 `open_id` 和 `union_id`。其中 `open_id` 通常以 `ou_` 开头，并且与具体自建应用绑定；同一个人在不同应用中的 `open_id` 不同，不能复制其他机器人的值。如果已经通过当前 AniPulse 应用获得了它，可以这样填写：

```text
FEISHU_RECEIVE_ID_TYPE=open_id
FEISHU_RECEIVE_ID=ou_xxxxxxxxxxxxxxxxxxxxxxxx
```

个人部署推荐优先使用 `email`，没有邮箱时再使用 `user_id`，通常不需要 `union_id`。无论使用哪一种，收件人都必须属于当前飞书组织，并包含在该应用已发布版本的“可用范围”内。具体参数格式见飞书官方的[发送消息接口](https://open.feishu.cn/document/server-docs/im-v1/message/create)和[通讯录常见问题](https://open.feishu.cn/document/ugTN1YjL4UTN24CO1UjN/uQzN1YjL0cTN24CN3UjN)。保存环境变量后，按第 7 节发送测试卡片验证配置。

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

### 从 Arch Linux 构建给 Ubuntu 使用的静态版本

不能把 Arch 上普通 `cargo build --release` 生成的 `target/release/anipulse` 直接复制到 Ubuntu。它会链接 Arch 的较新 glibc，在 Ubuntu 22.04 上通常报 `GLIBC_2.38 not found`。如果不想在服务器本机编译，应在 Arch 上生成完全静态的 musl 版本。

安装 musl 工具链并添加 Rust 目标：

```bash
sudo pacman -S --needed musl
rustup target add x86_64-unknown-linux-musl
```

项目包含由 C 编译器构建的 SQLite，因此需要同时指定 C 编译器和最终 linker。`musl-gcc` wrapper 与 rustc 默认 static PIE 的组合可能生成一个没有 `NEEDED` 共享库、却仍带 `/lib/ld-musl-x86_64.so.1` 解释器的异常文件；这种文件安装 musl loader 后也可能在启动时段错误。这是 Rust 已记录的 [`musl-gcc` static PIE 问题](https://github.com/rust-lang/rust/issues/95926)，构建时必须通过 `relocation-model=static` 禁用 PIE：

```bash
cd /path/to/anipulse
cargo test --locked
cargo clean --target x86_64-unknown-linux-musl

CC_x86_64_unknown_linux_musl=x86_64-linux-musl-gcc \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc \
RUSTFLAGS='-C target-feature=+crt-static -C relocation-model=static' \
cargo build --locked --release --target x86_64-unknown-linux-musl
```

上传前必须检查产物：

```bash
file target/x86_64-unknown-linux-musl/release/anipulse
readelf -l target/x86_64-unknown-linux-musl/release/anipulse | grep interpreter
readelf -d target/x86_64-unknown-linux-musl/release/anipulse | grep NEEDED
./target/x86_64-unknown-linux-musl/release/anipulse --help
```

`file` 应显示 `statically linked`；两条 `readelf` 命令都应该没有输出；`--help` 应能在 Arch 本机正常运行。只上传这个路径下的文件：

```text
target/x86_64-unknown-linux-musl/release/anipulse
```

该静态文件不依赖服务器的 glibc 或 musl loader。不要上传 Arch 原生的 `target/release/anipulse`，也不要在 Ubuntu 上把 glibc loader 软链接成 musl loader。

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

如果使用上一节在 Arch 生成的静态 musl 文件，应把第一条命令的源路径换成上传后的文件，例如：

```bash
sudo install -m 0755 /tmp/anipulse /usr/local/bin/anipulse
```

安装后先执行 `file /usr/local/bin/anipulse` 和 `/usr/local/bin/anipulse --help`，确认产物正确，再继续配置服务。

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
# 预留给鉴权网页候选审核通知；当前版本请保持 false。
request_timeout_secs = 15

[schedule]
bangumi_data_url = "https://unpkg.com/bangumi-data@0.3/dist/data.json"
bangumi_api_base_url = "https://api.bgm.tv"
preferred_site = "bilibili"
sync_interval_secs = 86400
failure_retry_secs = 900
request_timeout_secs = 30
sync_batch_size = 20
user_agent = "你的-Bangumi-用户名/AniPulse/0.1 (personal self-hosted)"
```

`provider = "feishu"` 和 `provider = "feishu_app"` 都代表应用机器人私聊。`channel` 是 AniPulse 用于通知幂等的稳定标识，部署后不要随意修改，否则同一 Episode 可能因新 channel 名称产生另一条通知记录。

Bangumi API 要求非浏览器客户端使用包含开发者个人标识和应用名的 User-Agent。把示例中的“你的-Bangumi-用户名”改成自己的用户名或稳定个人标识；自动排期不需要 Access Token。

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

管理类 CLI 不需要读取飞书 Secret。推荐让程序自动匹配条目、补全别名和更新时间：

```bash
sudo -u anipulse /usr/local/bin/anipulse \
  --config /etc/anipulse/config.toml \
  anime add \
  --title "沉默的魔女" \
  --next-episode 8 \
  --auto-schedule \
  --timezone Asia/Shanghai \
  --duration-min 20m \
  --duration-max 28m
```

自动匹配只接受标题的唯一精确匹配。同名作品、多季或重制版会返回候选 Bangumi ID，不会擅自选择；根据输出重新执行，例如：

```bash
sudo -u anipulse /usr/local/bin/anipulse \
  --config /etc/anipulse/config.toml \
  anime add \
  --title "沉默的魔女" \
  --next-episode 8 \
  --auto-schedule \
  --bangumi-id 506677
```

自动排期会每天同步一次；通知成功并创建下一集后会立即重新校准。也可以手动触发：

```bash
sudo -u anipulse /usr/local/bin/anipulse \
  --config /etc/anipulse/config.toml anime sync 1
```

如果数据源中没有该作品，仍可按原方式手工提供多个 `--alias`、`--weekday` 和 `--time`，但不要同时使用 `--auto-schedule`。

检查结果：

```bash
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml anime list
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml anime show 1
```

已入库标题可以直接修改，不用删除重加。例如把缺少空格的“无职转生第三季”改为更适合 Bilibili 搜索的标题：

```bash
sudo -u anipulse /usr/local/bin/anipulse \
  --config /etc/anipulse/config.toml \
  anime edit 4 \
  --title "无职转生 第三季"
```

旧标题会保留为别名，当前 Episode 会被安排为立即检查；常驻服务通常会在下一个 scheduler tick 开始搜索。可用 `anime show 4` 核对修改结果。

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

服务使用专用用户运行；`ProtectSystem=strict` 使系统目录只读，只允许写入 `/var/lib/anipulse`。它只需要通过 443 端口访问 Bilibili、`open.feishu.cn`、`unpkg.com` 和 `api.bgm.tv`，不需要在防火墙或路由器上开放入站端口。

## 10. 日常操作

禁用追番不会删除历史数据；适合暂停监控或先处理误添加记录：

```bash
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml anime disable 4
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml anime show 4
```

彻底删除前先核对目标。第一次不带 `--yes` 的调用会显示目标并拒绝执行；确认无误后再永久删除：

```bash
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml anime remove 4
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml anime remove 4 --yes
```

删除会在一个 SQLite 事务内完成，并通过外键级联清除该 Anime 的别名、Episode、候选、通知和 UP 信任数据，无法撤销。重要数据应先按“备份与恢复”一节创建备份。对于刚误加且担心调度器正在处理的记录，先执行 `anime disable ID`，再删除。

查看待确认候选及解释：

```bash
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml candidate list --state pending --explain
```

人工确认或拒绝：

```bash
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml candidate accept BVxxxxxxxxxx
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml candidate reject BVxxxxxxxxxx
sudo -u anipulse /usr/local/bin/anipulse --config /etc/anipulse/config.toml candidate reject-all 4 --yes
```

如果候选列表没有目标，但你已经知道 B 站视频地址，可以把规范链接直接作为当前集的人工确认：

```bash
sudo -u anipulse /usr/local/bin/anipulse \
  --config /etc/anipulse/config.toml \
  candidate accept-url 4 \
  'https://www.bilibili.com/video/BV1Es8A6UEnr'
```

该命令先调用视频详情接口验证 BV 号并保存真实标题、UP 和时长，再创建待发送通知。它只接受 BV 号或 `www.bilibili.com` / `m.bilibili.com` 的 HTTPS `/video/BV...` 地址，不接受短链和第三方域名。

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

### 二进制无法启动、提示 GLIBC 或 musl 段错误

- `GLIBC_2.38 not found`：使用了 Arch 原生 glibc 产物。改为服务器本机编译，或按第 3 节生成静态 musl 文件。
- `No such file or directory`，但文件实际存在：执行 `readelf -l /usr/local/bin/anipulse | grep interpreter`；通常是二进制仍要求系统中不存在的动态 loader。
- `readelf -d` 没有 `NEEDED`，但仍显示 `/lib/ld-musl-x86_64.so.1`，运行后段错误：命中了 `musl-gcc` static PIE 问题，不能靠安装 musl 修复；必须添加 `-C relocation-model=static` 后清理目标目录并重新编译。

可移植的静态文件必须同时满足：`file` 显示 `statically linked`，`readelf -l ... | grep interpreter` 没有输出，`readelf -d ... | grep NEEDED` 没有输出。服务器本机编译的 glibc 文件可以带 `/lib64/ld-linux-x86-64.so.2`，因为它与服务器自身的 glibc 匹配。

### 飞书没有收到测试卡片

先直接运行第 7 节的测试命令，再查看退出信息：

- `Feishu authentication failed`：核对 App ID 与 App Secret；修改 Secret 后重新写入环境文件。
- `Feishu message failed` 且提示权限不足：确认已经申请 `im:message:send_as_bot`，并在申请权限后重新发布应用。
- 提示机器人无法访问用户：确认应用可用范围包含你，并重新发布应用。
- 提示收件人无效：确认邮箱来自当前组织通讯录；或者改用 `open_id` 与 `ou_...`。
- HTTP timeout/connect：检查服务器能否通过 443 端口访问 `open.feishu.cn`。

程序会缓存飞书访问令牌，并在过期前自动刷新，不需要把 `tenant_access_token` 写入环境文件。

### 自动排期匹配或同步失败

- 多条同名记录：根据命令列出的候选选择正确季度，再传入 `--bangumi-id`。
- 找不到精确标题：换用作品的正式中文名或日文原名；仍找不到时改用手工排期。
- Bangumi 请求失败：检查服务器能否访问 `unpkg.com` 和 `api.bgm.tv`，以及 `schedule.user_agent` 是否已经修改。

后台同步失败不会清空现有时间；程序会保留旧排期，默认 15 分钟后重试。可用 `anime show ID` 查看最近同步时间和错误。

### 通知一直是 pending

发送失败时 AniPulse 会记录简化错误、保留 pending，并按指数退避再次发送。修复飞书配置后无需修改数据库，等待重试或重启服务即可。

### Bilibili 返回 412/429 或 `v_voucher`

这是 Provider 风控或限流。新版 AniPulse 会识别 `code=0` 但仅含 `v_voucher` 的软风控响应，不会再把它记录成正常的 `results=0`；公开聚合搜索响应结构异常时会尝试 WBI 回退，任一路径明确返回风控时都会进入持久化全局 backoff。期间不会持续高频请求。不要通过代理池、Cookie 池或高频重启绕过。

如果旧日志连续显示 `search completed results=0`，但浏览器能够搜到目标视频，先升级到包含软风控识别和聚合搜索支持的版本，再手工执行一次 `check ID`。升级不会要求重新添加追番，也不会清空现有 SQLite 数据。

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
