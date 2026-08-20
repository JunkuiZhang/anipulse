# AniPulse

AniPulse 是一个面向个人 Linux 服务器的番剧更新监控器。它低频搜索 Bilibili 公共视频，只在“可信 UP”或“不同 UP 的独立共识”成立时自动确认；搜索结果和分数本身都不会直接触发通知。

完整需求边界见 [`doc/thoughts.md`](doc/thoughts.md)，实现阶段和验收标准见 [`doc/implementation-plan.md`](doc/implementation-plan.md)。

## 当前 V1 能力

- SQLite 持久化 Anime、Episode、Candidate、UP 信任、通知和全局 Provider 退避；
- 标题规范化、番名匹配、整数 Episode 识别、时长/发布时间/负面标题多信号评分；
- Candidate 按 Episode + BV 去重，同一 MID 永远只算一个 Consensus vote；
- Trusted Uploader、Independent Consensus、Manual Confirmation 三条确认路径；
- WBI 搜索和视频详情 Provider 隔离；全局串行限流、每日预算、429/412/临时失败退避；
- 飞书消息卡片通知、幂等重试、下一集推进、动态轮询、jitter 和 systemd 常驻运行；
- CLI 人工 accept/reject 与 per-Anime UP trust/block。

V1 不下载视频、不使用登录 Cookie、不绕过风控，也不处理 `EP12.5`、SP、OVA、连播或分 P 的自动推进。此类标题只进入人工复核。

## 构建

需要稳定版 Rust：

```bash
cargo build --release
cargo test
```

复制配置：

```bash
cp config.example.toml config.toml
```

不指定配置或配置文件不存在时，CLI 会使用安全默认值，并把数据库放在当前目录的 `anipulse.db`。生产环境应显式使用 `/etc/anipulse/config.toml`。

## CLI 快速开始

添加一部番并创建正在等待的 EP8：

```bash
anipulse anime add \
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

常用命令：

```bash
anipulse anime list
anipulse anime show 1
anipulse check 1
anipulse candidate list --state pending --explain
anipulse candidate accept BVxxxxxxxxxx
anipulse candidate reject BVxxxxxxxxxx
anipulse uploader trust 1 123456
anipulse uploader block 1 123456
anipulse notification test
anipulse run
```

`candidate accept` 只事务化确认状态并创建 pending notification；下一次 `run` 或 `check` 会发送它。这样即使通知服务暂时失败也不会丢失已确认更新。

## 通知

生产配置推荐使用飞书自建应用机器人，直接私聊你的飞书账号：

```toml
[notification]
provider = "feishu"
channel = "feishu-private-anime"
```

应用凭证和收件人只通过环境变量提供。收件人使用飞书账号邮箱最省事：

```bash
export FEISHU_APP_ID='cli_xxxxxxxxx'
export FEISHU_APP_SECRET='应用的 App Secret'
export FEISHU_RECEIVE_ID_TYPE='email'
export FEISHU_RECEIVE_ID='你的飞书账号邮箱'
anipulse notification test
```

更新确认后，应用机器人会直接向你发送包含确认依据、UP、时长、BV 号和“立即观看”按钮的消息卡片。程序缓存 `tenant_access_token` 并在过期前刷新；无需事件订阅、回调地址或服务器入站端口。不要把 App Secret、Cookie 或其他 secret 写入仓库，日志也不会打印这些值。

原有飞书群自定义机器人仍可作为兼容选项：把 `provider` 设为 `feishu_webhook`，并提供 `FEISHU_WEBHOOK_URL` 以及可选的 `FEISHU_BOT_SECRET`。

## Linux + systemd 部署

示例以专用系统用户运行：

```bash
sudo useradd --system --home /var/lib/anipulse --shell /usr/sbin/nologin anipulse
sudo install -m 0755 target/release/anipulse /usr/local/bin/anipulse
sudo install -d -m 0755 /etc/anipulse
sudo install -m 0644 config.example.toml /etc/anipulse/config.toml
sudo install -m 0644 deploy/anipulse.service /etc/systemd/system/anipulse.service
```

创建仅 root 可读的飞书环境文件：

```bash
sudoedit /etc/anipulse/anipulse.env
sudo chmod 600 /etc/anipulse/anipulse.env
```

启动并查看日志：

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now anipulse
journalctl -u anipulse -f
```

环境文件内容为：

```text
FEISHU_APP_ID=cli_xxxxxxxxx
FEISHU_APP_SECRET=应用的-App-Secret
FEISHU_RECEIVE_ID_TYPE=email
FEISHU_RECEIVE_ID=你的飞书账号邮箱
RUST_LOG=info
```

从创建飞书自建应用、开通私聊权限、构建 Linux 二进制到备份升级的完整步骤见 [`doc/deployment.md`](doc/deployment.md)。

## 判定语义

自动确认必须满足以下之一：

1. 该 Anime 下的可信 UP，且番名、集数、时长正常，没有负面标题信号；
2. 至少两个不同 MID 的候选，集数强匹配、时长相近、发布时间接近且各自达到最低分数；
3. 用户执行 `candidate accept`。

每个候选都保存结构化 `evaluation_json`，可用 `--explain` 查看。无法确定时保持 Pending；这是系统最优先的业务规则。

## 外部接口边界

Bilibili Web API 不是本项目可控制的稳定接口。所有 endpoint、WBI 签名、响应字段和错误码映射集中在 `src/provider/bilibili.rs`；如果接口变化，应只修改 Provider。遇到 HTTP 429、HTTP/Bilibili 412 或异常响应时，AniPulse 不会高频重试。
