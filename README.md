# AniPulse

AniPulse 是一个面向个人 Linux 服务器的番剧更新监控器。它低频搜索 Bilibili 公共视频，只在“可信 UP”或“不同 UP 的独立共识”成立时自动确认；搜索结果和分数本身都不会直接触发通知。

完整需求边界见 [`doc/thoughts.md`](doc/thoughts.md)，实现阶段和验收标准见 [`doc/implementation-plan.md`](doc/implementation-plan.md)。

## 当前 V1 能力

- SQLite 持久化 Anime、Episode、Candidate、UP 信任、通知和全局 Provider 退避；
- 标题规范化、番名匹配、整数 Episode 识别、时长/发布时间/负面标题多信号评分；
- Candidate 按 Episode + BV 去重，同一 MID 永远只算一个 Consensus vote；
- Trusted Uploader、Independent Consensus、Manual Confirmation 三条确认路径；
- 公共聚合搜索、WBI 回退和视频详情 Provider 隔离；识别 `v_voucher` 软风控响应，并执行全局串行限流、每日预算及 429/412/临时失败退避；
- 飞书消息卡片通知、幂等重试、下一集推进、动态轮询、jitter 和 systemd 常驻运行；
- CLI 改名、状态回退修复、按 B 站链接人工确认、候选 accept/reject 与 per-Anime UP trust/block；
- 单管理员鉴权网页：Dashboard、卡片式追番、添加/启停/改名、播完待看与归档收藏、安全删除、候选审核、过滤与信任规则、后台任务与审计；
- 无法自动确认时发送飞书私聊审核卡片，登录网页后选择候选、都不选或提交链接。

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

推荐自动匹配 Bangumi 条目、补全别名和播出时间，然后创建正在等待的 EP8：

```bash
anipulse anime add \
  --title "沉默的魔女" \
  --next-episode 8 \
  --auto-schedule \
  --duration-min 20m \
  --duration-max 28m
```

如果同名条目对应多季或重制版，命令会拒绝静默选择并列出候选 ID；重新执行时添加 `--bangumi-id 506677`。自动排期每天重新读取 `bangumi-data`，并用 Bangumi 章节日期校准当前集；也可执行 `anipulse anime sync 1` 立即同步。手工 `--weekday/--time` 仍然可用，但与 `--auto-schedule` 互斥。

未上映作品可能已经存在于 Bangumi、但尚未进入 `bangumi-data`。此时填写 `--bangumi-id` 后，AniPulse 会用 Bangumi 日文标题、首播日期和类型匹配 AniList；只有唯一高置信结果才会自动绑定，并保存 AniList Media ID。匹配不唯一时会列出候选，核对后加 `--anilist-id ID` 重试。AniList 没有发布具体 `nextAiringEpisode` 时不会臆造时刻，可先使用手工排期，等上游补全后再同步。

如果站内使用的集数与 Bangumi 条目编号不同，可以提供一组起点映射。例如站内 EP12 对应 Bangumi EP78：

```bash
anipulse anime add \
  --title "Re：从零开始的异世界生活 第四季 夺还篇" \
  --next-episode 14 \
  --auto-schedule \
  --bangumi-id 633836 \
  --search-episode-start 12 \
  --bangumi-episode-start 78
```

此时 Bilibili 仍搜索 EP14，排期则查询该 Bangumi 条目的第 3 个章节并校验为 EP80。网页添加页和番剧详情页也可以填写或修改同一组映射。

常用命令：

```bash
anipulse anime list
anipulse anime show 1
anipulse anime edit 1 --title "无职转生 第三季"
anipulse anime sync 1
anipulse anime disable 4
anipulse anime repair-episode 1 --episode 9 --yes
anipulse anime remove 4 --yes
anipulse check 1
anipulse candidate list --state pending --explain
anipulse candidate accept BVxxxxxxxxxx
anipulse candidate accept-url 1 https://www.bilibili.com/video/BVxxxxxxxxxx
anipulse candidate reject BVxxxxxxxxxx
anipulse candidate reject-all 1 --yes
anipulse uploader trust 1 123456
anipulse uploader block 1 123456
anipulse notification test
anipulse run
```

`anime remove ID` 是永久删除操作，会级联清除该番剧的别名、Episode、候选、通知和 UP 信任记录。命令默认拒绝执行；必须先用 `anime show ID` 核对目标，再显式添加 `--yes`。误添加时先 `anime disable ID` 可立即阻止后台继续调度。

`anime edit ID --title "新标题"` 会修改首选搜索标题、保留旧标题作为别名，并把当前 Episode 调整为立即检查。空格会影响 Bilibili 的搜索召回；例如已入库的“无职转生第三季”可直接改成“无职转生 第三季”，不需要删除重加。

`candidate accept` 只事务化确认状态并创建 pending notification；下一次 `run` 或 `check` 会发送它。这样即使通知服务暂时失败也不会丢失已确认更新。

候选写操作以 `Episode ID + BV 号` 为完整身份。确认某一集后，同集其余待审核候选会立即过期；旧页面或旧飞书审核入口不能拿上一集视频推进当前集。升级 migration 也会清理旧版本遗留在已完成 Episode 下的 pending candidate。

如果旧版本已经把错误视频当成 EP9 推送并推进到 EP10，不要直接编辑 SQLite。先停止 scheduler、备份数据库并禁用该番剧，再执行 `anime repair-episode ANIME_ID --episode 9 --yes`；命令会删除 EP9 的错误通知记录、使其错误候选过期、删除 EP10 及更后面的派生状态，把 EP9 恢复为立即检查。错误的飞书卡片无法撤回，但数据库状态会恢复。完整命令见部署文档。

如果搜索没有发现目标视频，但你已经拿到规范的 Bilibili 视频地址，可执行 `candidate accept-url ANIME_ID URL`。AniPulse 会先通过 Bilibili 详情接口校验 BV 号并保存标题、UP、时长等元数据，再把它作为当前等待集数的人工确认候选；它不接受第三方域名或任意 URL。

如果当前集的待确认候选全都不对，可在先查看列表后执行 `candidate reject-all ANIME_ID --yes`。它只拒绝该 Anime 当前 Episode 的 pending candidates，并逐个记录人工反馈，不影响之后新发现的候选。

## 网页管理端

网页和 scheduler 是两个独立进程。生产环境让网页只监听 `127.0.0.1:8080`，由 Caddy 提供 HTTPS；管理员必须从服务器 TTY 创建，没有默认账号或网页注册：

```bash
anipulse database migrate
anipulse auth admin create --username admin
anipulse web
```

网页 Secret 只通过 `ANIPULSE_WEB_SECRET` 提供，飞书 Secret 仍只交给 `anipulse run`。完整的域名、Caddy、systemd、管理员恢复和审核提醒部署步骤见 [`doc/web-deployment.md`](doc/web-deployment.md)。

网页“规则”页可以全局维护屏蔽词，并按番剧维护信任 UP。屏蔽词会在后续检查中匹配视频标题、简介和标签并硬排除候选；信任 UP 仍必须通过番名、目标集数、时长和屏蔽词检查，不是无条件放行。

网页时间由 `[web].timezone` 统一控制，默认 `Asia/Shanghai`。数据库仍保存 UTC，修改显示时区不需要迁移数据。

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

飞书通道保持单向通知：机器人可以私聊发卡片，但不接收卡片回调或你回复的文本。启用 `notify_pending` 后，不确定候选卡片会打开登录后的网页审核页，由页面执行“确认一个、全部拒绝、提交 B 站链接”。这样无需暴露一个可匿名修改数据库的飞书回调；部署见 [`doc/web-deployment.md`](doc/web-deployment.md)。

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

鉴权网页的架构、威胁模型、数据表、路由和验收标准见 [`doc/web-management-plan.md`](doc/web-management-plan.md)，生产部署教程见 [`doc/web-deployment.md`](doc/web-deployment.md)。

## 判定语义

自动确认必须满足以下之一：

1. 该 Anime 下的可信 UP，且番名、集数、时长正常，没有负面标题信号；
2. 至少两个不同 MID 的候选，集数强匹配、时长相近、发布时间接近且各自达到最低分数；
3. 用户执行 `candidate accept`。

每个候选都保存结构化 `evaluation_json`，可用 `--explain` 查看。无法确定时保持 Pending；这是系统最优先的业务规则。

## 外部接口边界

Bilibili Web API 不是本项目可控制的稳定接口。所有 endpoint、WBI 签名、响应字段和错误码映射集中在 `src/provider/bilibili.rs`；如果接口变化，应只修改 Provider。Provider 优先读取公开聚合搜索中的 video 分组，在响应不可用时才尝试 WBI 搜索；`code=0` 但只含 `v_voucher` 会被识别为软风控，不再误报成“0 条结果”。遇到 HTTP 429、HTTP/Bilibili 412、软风控或异常响应时，AniPulse 不会高频重试。

自动排期数据来自 [bangumi-data](https://github.com/bangumi-data/bangumi-data)（CC BY 4.0）和 [Bangumi API](https://bangumi.github.io/api/)。AniPulse 把 Bangumi 单集日期当作日期锚点，并优先用 `bangumi-data` 中可信网络平台的时段校准跨日、网络先行和电视/网络时间差；服务器不会直接访问这些平台的网站。尚未进入目录的未上映作品可额外使用 [AniList GraphQL API](https://docs.anilist.co/) 的精确开播时刻，映射会持久化并在目录正式收录后自动切回常规来源。无法安全对齐时会隐藏预计时间而不是拼出一个错误的精确值，Bilibili 检查仍继续。外部元数据只用于缩小检查窗口，不会单独触发“已更新”通知。

`bangumi-data`、同一条目的章节日期，或未上映条目的 Bangumi/AniList fallback 连续两次异常时，AniPulse 会通过当前通知通道发送一次数据源告警；同一轮故障不会重复打扰，恢复后会再发送一次恢复通知。详情页和 `anime show` 会显示排期来源、校准状态和降级原因。配置含义、升级步骤与冲突处理见 [`doc/deployment.md`](doc/deployment.md)。

国内服务器无法稳定直连这些元数据服务时，可以部署项目自带的专用 [Cloudflare Worker 转发服务](doc/cloudflare-worker.md)。它只开放 AniPulse 所需的固定只读路由、限制服务器出口 IP、重建允许的 AniList 查询、在 Worker 内跟随封面重定向并分层缓存，不会成为开放代理。
