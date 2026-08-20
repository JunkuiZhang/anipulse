# Anime Release Watcher V1 技术设计规格

## 1. 项目目标

实现一个运行在个人 Linux 服务器上的番剧更新监控服务。

用户配置自己正在追的番剧以及当前等待的集数。服务周期性搜索 Bilibili 上的公开视频，判断目标番剧的新一集是否已经出现。

搜索结果不能直接视为更新。系统必须经过：

**候选发现 → 规则过滤 → 元数据验证 → 多信号判定 → 更新确认 → 推送通知**

完整流程后，才能发送“番剧已更新”的通知。

核心目标：

1. 尽量及时发现正在追的番剧出现新一集；
2. 最大限度降低标题党、解说、预告、剪辑、假正片等造成的误报；
3. 请求频率必须非常克制，避免对 Bilibili 造成高频请求以及触发风控；
4. Bilibili API 发生变化时，只修改 Provider，不影响核心业务逻辑；
5. 所有判定必须可解释；
6. 支持用户人工纠正错误，并利用历史结果提高以后判断准确率；
7. 服务能够长期无人值守运行。

系统优化目标为：

> **False Positive 的代价高于通知晚 10～30 分钟。**

即宁愿稍晚确认，也不要看到一个疑似视频就立即通知。

---

# 2. 明确的非目标

V1 不实现以下功能。

## 2.1 不下载视频

系统只获取公开视频的搜索结果和 metadata。

禁止实现：

* 视频下载；
* 音视频流解析；
* 视频缓存；
* 视频重新分发；
* 视频内容识别；
* 视频帧分析。

系统只保存：

* BV ID；
* 标题；
* UP 主；
* 发布时间；
* 视频时长；
  -简介；
* tags；
* URL；
* 搜索和判断结果。

---

## 2.2 不绕过 Bilibili 风控

禁止：

* IP 池；
  -代理池；
* Cookie 池；
  -自动验证码；
  -浏览器指纹伪装；
  -高频重试；
  -风控绕过逻辑。

遇到：

* HTTP 429；
* Bilibili `412`；
  -明显异常响应；

必须执行全局退避。

---

## 2.3 不保证 100% 判断视频内容真伪

只根据公开视频 metadata 判断。

因此系统无法绝对证明：

> “这个 23:40 的视频内部一定是真正的动画 EP08。”

一个恶意上传者理论上可以伪造：

* 标题；
  -时长；
  -简介；
  -tags；
  -发布时间。

系统只能通过多个独立信号降低误报概率。

这属于项目的根本边界。

---

## 2.4 V1 不解决所有特殊 Episode

V1 完整支持：

```text
EP1
EP2
EP3
...
EP12
...
```

即整数 Episode。

以下情况暂不自动推进：

```text
EP12.5
SP
OVA
总集篇
特别篇
上下篇
连续两集
EP8-9
分 P 上传
一条视频包含多集
```

发现这些情况可以保存 Candidate，但进入：

```text
NeedsManualReview
```

不得自动修改当前 Episode。

后续版本再设计统一 `EpisodeKey`。

---

# 3. 总体架构

```text
                         ┌──────────────────┐
                         │     Bangumi      │
                         │ optional metadata│
                         └────────┬─────────┘
                                  │
                                  ▼
                         ┌──────────────────┐
                         │      Anime       │
                         │     Metadata     │
                         └────────┬─────────┘
                                  │
                                  ▼
┌────────────────┐       ┌──────────────────┐
│    Scheduler   │──────▶│ Release Detector │
└────────────────┘       └────────┬─────────┘
                                  │
                                  ▼
                         ┌──────────────────┐
                         │ Bilibili Provider│
                         └────────┬─────────┘
                                  │
                                  ▼
                         Search Result
                                  │
                                  ▼
                         ┌──────────────────┐
                         │ Candidate Filter │
                         └────────┬─────────┘
                                  │
                                  ▼
                         ┌──────────────────┐
                         │ Metadata Enricher│
                         └────────┬─────────┘
                                  │
                                  ▼
                         ┌──────────────────┐
                         │ Candidate Judge  │
                         └────────┬─────────┘
                                  │
                         ┌────────┴────────┐
                         │                 │
                       reject           pending
                                           │
                                           ▼
                                     next polling
                                           │
                                           ▼
                                  consensus formed
                                           │
                                           ▼
                                      confirmed
                                           │
                                           ▼
                         ┌──────────────────┐
                         │     SQLite       │
                         └────────┬─────────┘
                                  │
                                  ▼
                         ┌──────────────────┐
                         │     Notifier     │
                         └────────┬─────────┘
                                  │
                                  ▼
                               微信等
```

---

# 4. 技术栈

推荐：

```text
Language     Rust
Runtime      Tokio
HTTP         reqwest
JSON         serde / serde_json
Database     SQLite
Database     sqlx
CLI          clap
Logging      tracing
Config       TOML
Time         chrono
HTTP server  axum（V1 暂时不需要）
```

部署：

```text
Linux
systemd
```

服务采用：

```text
anime-watcher.service
```

常驻运行。

不采用复杂 Scheduler 框架。

程序内部维护一个非常简单的循环：

```text
tick every 30 seconds

SELECT episodes
WHERE next_check_at <= now()
```

然后执行需要检查的 Episode。

---

# 5. 为什么使用常驻进程

此前可以考虑 systemd timer 每几分钟启动一次程序，但最终 V1 建议使用常驻进程。

原因是系统存在：

* 动态 `next_check_at`；
* Candidate pending；
* Consensus 等待；
  -不同 Episode 不同检查周期；
  -全局 Bilibili Rate Limiter；
  -退避状态；
  -推送重试。

常驻进程反而更简单。

systemd 只负责：

```text
启动
崩溃重启
日志
开机启动
```

---

# 6. 核心领域模型

系统主要包含以下概念：

```text
Anime
Episode
Candidate
UploaderTrust
Notification
```

---

# 7. Anime

```rust
struct Anime {
    id: i64,

    title: String,

    aliases: Vec<String>,

    bangumi_subject_id: Option<i64>,

    expected_weekday: Option<Weekday>,

    expected_time: Option<NaiveTime>,

    timezone: String,

    normal_duration_min_sec: u32,
    normal_duration_max_sec: u32,

    enabled: bool,
}
```

例如：

```text
title:
Silent Witch

aliases:
- Silent Witch
- 沉默魔女的秘密
- 沉默的魔女
- サイレント・ウィッチ

normal_duration:
1200 ~ 1800 sec
```

alias 必须允许人工编辑。

不要认为 Bangumi 给出的所有别名都适合 Bilibili 搜索。

---

# 8. Episode

每一集建立一个独立 Episode 状态。

```rust
enum EpisodeState {
    Waiting,
    Watching,
    CandidateFound,
    Confirmed,
    Notified,
    NeedsManualReview,
}
```

数据：

```rust
struct Episode {
    id: i64,

    anime_id: i64,

    episode_no: u32,

    expected_at: Option<DateTime<Utc>>,

    state: EpisodeState,

    first_candidate_at: Option<DateTime<Utc>>,

    confirmed_at: Option<DateTime<Utc>>,

    notified_at: Option<DateTime<Utc>>,

    next_check_at: DateTime<Utc>,
}
```

一个 Anime 同一时间原则上只有一个 active Episode。

例如：

```text
Anime:
Silent Witch

current target:
EP08

EP08.state = Watching
```

EP08 Confirmed 后：

```text
EP08 → Notified

自动创建：

EP09 → Waiting
```

---

# 9. Candidate

Bilibili 搜索结果统一转换为：

```rust
struct VideoCandidate {
    bvid: String,

    title: String,

    description: Option<String>,

    uploader_mid: u64,

    uploader_name: String,

    duration_sec: u32,

    published_at: DateTime<Utc>,

    url: String,

    tags: Vec<String>,

    page_count: Option<u32>,

    discovered_at: DateTime<Utc>,
}
```

业务层不得直接依赖 Bilibili JSON。

---

# 10. Provider 抽象

定义：

```rust
#[async_trait]
trait VideoSearchProvider {
    async fn search(
        &self,
        query: &SearchQuery,
    ) -> Result<Vec<VideoCandidate>>;

    async fn enrich(
        &self,
        candidate: &VideoCandidate,
    ) -> Result<VideoCandidate>;
}
```

Bilibili 实现：

```text
BilibiliSearchProvider
```

负责：

```text
HTTP
Cookie
WBI
JSON
错误码
字段转换
```

Detector 不允许知道：

```text
/x/web-interface/...
```

这种具体 endpoint。

---

# 11. Bilibili Provider 的边界

Bilibili Web API 并非本项目可控制的稳定接口。

因此必须假设未来可能发生：

```text
URL 改变
参数改变
需要 WBI
Cookie 要求改变
返回字段改变
请求限制改变
API 被关闭
```

Provider 必须把这些变化隔离。

定义统一错误：

```rust
enum ProviderError {
    RateLimited,
    RiskControl,
    Unauthorized,
    Temporary,
    InvalidResponse,
    Permanent,
}
```

业务层只处理这些错误。

---

# 12. 搜索策略

不要一部番一次发 5～10 个搜索请求。

每轮最多：

```text
1 个主要 query
+
必要时 1 个 fallback query
```

例如：

```text
Silent Witch 8
```

如果没有有效候选：

```text
沉默魔女 8
```

不要同时搜索：

```text
Silent Witch EP8
Silent Witch EP08
Silent Witch 08
Silent Witch 第八集
沉默魔女 EP8
沉默魔女 08
...
```

Episode 格式差异应该由本地 Parser 处理，而不是制造更多搜索请求。

---

# 13. Candidate Pipeline

每次搜索：

```text
search
   ↓
normalize
   ↓
deduplicate by BV ID
   ↓
cheap filter
   ↓
candidate scoring
   ↓
仅对有希望的 candidate 请求 detail
   ↓
re-score
   ↓
store
   ↓
confirmation logic
```

例如搜索返回 20 个：

```text
20 search results
        ↓
hard filter
        ↓
4 candidates
        ↓
score
        ↓
2 interesting candidates
        ↓
fetch detail
```

不要对所有 20 个视频调用 detail API。

---

# 14. 标题标准化

实现：

```rust
fn normalize_title(title: &str) -> String
```

至少处理：

```text
Unicode normalization
HTML tag removal
大小写
全角半角
连续空格
常见标点
搜索结果中的 <em> tag
```

例如：

```text
【1080P】Silent Witch 08
```

规范化后仍要保留足够结构用于 Episode Parser。

不要直接把所有非字母数字字符删除，因为：

```text
8-9
12.5
```

具有语义。

---

# 15. Episode Parser

实现独立模块：

```rust
EpisodeMatcher
```

目标 Episode：

```text
8
```

可识别：

```text
EP8
EP08
E8
E08
Episode 8
第8集
第08集
第八集
08
```

必须避免：

```text
1080P
18+
2026
8月
80
18
```

被误认为 EP8。

匹配应该输出：

```rust
enum EpisodeMatch {
    Strong,
    Weak,
    None,
    Ambiguous,
}
```

例如：

```text
Silent Witch EP08

Strong
```

```text
Silent Witch 08

Strong
```

```text
Silent Witch 8

Weak / Strong
取决于上下文
```

```text
Silent Witch 1080P

None
```

---

# 16. 番名匹配

至少有一个 Anime Alias 必须匹配 Candidate。

输出：

```rust
enum AnimeMatch {
    Exact,
    Strong,
    Weak,
    None,
}
```

V1 不需要 fuzzy embedding。

简单：

```text
normalized substring
token overlap
```

即可。

避免：

```text
Levenshtein 随便匹配
```

导致其它番误入。

---

# 17. 硬过滤规则

以下情况可以直接 Reject。

## 17.1 Episode 明确不匹配

等待：

```text
EP08
```

Candidate 明确：

```text
EP07
EP09
EP18
```

直接 Reject。

---

## 17.2 视频明显过短

Anime：

```text
normal_duration:
20 ~ 27 min
```

如果：

```text
duration < 8 min
```

基本可以直接 Reject。

不要严格要求：

```text
20 <= duration <= 27
```

因为可能存在：

```text
18:30
28:10
```

这种真实正片。

建议区分：

```text
hard minimum
normal range
```

例如：

```text
hard_min = 60% * expected_min
```

---

## 17.3 明确负面关键词

默认配置：

```text
预告
PV
预热视频
reaction
REACTION
解说
解析
吐槽
名场面
MAD
AMV
剪辑
速看
一口气
预测
```

这些词不是全部 Hard Reject。

例如：

```text
无解说完整版
```

包含“解说”。

因此实现：

```text
negative score
```

而不是所有关键词直接 Reject。

真正明显的：

```text
EP8 预告
```

可以 Reject。

---

# 18. Candidate Score

评分只用于：

1. 排序；
2. 过滤明显无意义的 Candidate；
3. 为 Confirmation Rule 提供辅助信息。

不要简单写：

```text
score >= 90
=> Confirmed
```

最终确认必须经过 Confirmation Rule。

建议初始权重：

```text
Anime exact match                 +20

Episode strong                    +30
Episode weak                      +15
Episode ambiguous                 -20

duration normal                   +15
duration slightly outside          +5
duration suspicious               -30

publication near expected         +10

known trusted uploader            +25

negative keyword                  -20 ~ -60

metadata/detail consistent        +10
```

Score 范围无需强制 0～100。

---

# 19. 发布时间信号

预计：

```text
Friday 23:00
```

Candidate 发布时间：

```text
Friday 22:45
```

这是强正信号。

但发布时间绝不能做严格 Hard Gate。

因为：

```text
提前放送
字幕提前
投稿审核延迟
时区
特殊更新
```

都可能造成变化。

建议：

```text
expected_at - 12h
~
expected_at + 48h
```

视为正常窗口。

明显早很多：

```text
expected_at - 3 days
```

给予负分。

但是仍可以进入 Candidate。

---

# 20. Uploader Trust

这是系统最重要的长期信号之一。

保存：

```rust
struct UploaderTrust {
    anime_id: i64,
    uploader_mid: u64,

    confirmed_count: u32,
    rejected_count: u32,

    manually_trusted: bool,
    manually_blocked: bool,
}
```

Trust 必须是：

```text
per Anime
```

而不是全局。

因为一个 UP：

```text
经常搬运 Anime A
```

并不意味着：

```text
Anime B 也可靠
```

---

# 21. Uploader Trust 规则

用户人工确认 Candidate 为真：

```text
confirmed_count += 1
```

用户人工确认是假：

```text
rejected_count += 1
```

自动 Confirmed 不应该无限自我强化。

建议：

```text
只有用户实际点击“正确”
或者
该视频后来获得非常强 Consensus
```

才增加主要 Trust。

否则容易形成错误反馈环：

```text
算法错误确认
↓
UP trust 增加
↓
下一次更容易错误确认
```

---

# 22. 多上传者 Consensus

这是陌生上传者情况下最重要的确认机制。

如果：

```text
Candidate A
UP A
EP08
23:41

Candidate B
UP B
EP08
23:39
```

满足：

```text
不同 uploader_mid
相同 Anime
相同 Episode
均通过 Hard Gate
时长接近
发布时间接近
```

可以形成 Consensus。

推荐条件：

```text
distinct uploaders >= 2

AND

每个 candidate score >= minimum_candidate_score

AND

duration difference <= 180 sec

AND

publication difference <= 120 min
```

以上参数均放 Config，不写死。

---

# 23. 为什么 Consensus 有价值

单个假视频可以伪造：

```text
title
duration
description
tags
```

但两个独立 UP 在同一时间发布：

```text
相同番
相同 Episode
相似时长
```

是更强的证据。

不过 Consensus 仍然不是数学意义的真实性证明。

两个账号也可能：

```text
同时转发同一个假资源
复制标题
重复投稿
```

因此它只是强信号。

---

# 24. Confirmation Rule

V1 不允许只靠 Score Confirm。

建议只有下面三条路径。

---

## Path A：Trusted Uploader

满足：

```text
AnimeMatch >= Strong
EpisodeMatch == Strong
duration acceptable
no strong negative signal

AND

uploader is trusted for this Anime
```

则：

```text
Confirmed
```

Trusted 定义建议：

```text
manually_trusted
OR
confirmed_count >= 3 && rejected_count == 0
```

---

## Path B：Independent Consensus

满足：

```text
>= 2 independent uploaders

Anime match
Episode strong
duration approximately equal
publication time close
candidate scores acceptable
```

则：

```text
Confirmed
```

---

## Path C：Manual Confirmation

如果长期只有一个陌生 Candidate：

```text
Pending
```

可以选择发送：

```text
疑似更新
```

但默认配置：

```text
notify_pending = false
```

即 V1 默认不打扰用户。

CLI 中允许：

```text
anime-watcher candidates
anime-watcher accept <bvid>
anime-watcher reject <bvid>
```

---

# 25. Candidate 状态

```rust
enum CandidateState {
    Pending,
    Confirmed,
    Rejected,
    Expired,
}
```

Candidate 初次出现：

```text
Pending
```

以后每轮搜索仍然看到：

```text
更新 last_seen_at
seen_count += 1
```

注意：

```text
同一个 BV 被连续看见 10 次
```

不能视作 Consensus。

Consensus 必须：

```text
不同 uploader_mid
```

---

# 26. Candidate 生命周期

建议：

```text
first_seen_at
last_seen_at
seen_count
```

Candidate 超过：

```text
72h
```

仍然未确认，可以：

```text
Expired
```

不要永久保留为 active。

数据库记录可以继续存在。

---

# 27. Release Detection 状态机

完整状态：

```text
Waiting
   │
   │ approaching release
   ▼
Watching
   │
   │ candidate found
   ▼
CandidateFound
   │
   ├────────── no valid candidate ──────┐
   │                                    │
   │                                    ▼
   │                                  Watching
   │
   │ confirmation rule satisfied
   ▼
Confirmed
   │
   │ notification successful
   ▼
Notified
   │
   ▼
Create next Episode
```

需要保证：

```text
Confirmed
```

和：

```text
Notification sent
```

是两个不同状态。

否则推送失败后可能丢失更新。

---

# 28. 数据库 Schema

推荐 SQLite migration。

## anime

```sql
CREATE TABLE anime (
    id INTEGER PRIMARY KEY AUTOINCREMENT,

    title TEXT NOT NULL,

    bangumi_subject_id INTEGER,

    expected_weekday INTEGER,
    expected_time TEXT,
    timezone TEXT NOT NULL DEFAULT 'Asia/Shanghai',

    duration_min_sec INTEGER NOT NULL,
    duration_max_sec INTEGER NOT NULL,

    enabled INTEGER NOT NULL DEFAULT 1,

    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
```

---

## anime_alias

```sql
CREATE TABLE anime_alias (
    id INTEGER PRIMARY KEY AUTOINCREMENT,

    anime_id INTEGER NOT NULL,

    alias TEXT NOT NULL,

    priority INTEGER NOT NULL DEFAULT 0,

    enabled INTEGER NOT NULL DEFAULT 1,

    UNIQUE(anime_id, alias)
);
```

---

## episode

```sql
CREATE TABLE episode (
    id INTEGER PRIMARY KEY AUTOINCREMENT,

    anime_id INTEGER NOT NULL,

    episode_no INTEGER NOT NULL,

    expected_at TEXT,

    state TEXT NOT NULL,

    next_check_at TEXT NOT NULL,

    first_candidate_at TEXT,

    confirmed_at TEXT,

    notified_at TEXT,

    UNIQUE(anime_id, episode_no)
);
```

---

## candidate

```sql
CREATE TABLE candidate (
    id INTEGER PRIMARY KEY AUTOINCREMENT,

    episode_id INTEGER NOT NULL,

    bvid TEXT NOT NULL,

    uploader_mid INTEGER NOT NULL,
    uploader_name TEXT NOT NULL,

    title TEXT NOT NULL,
    description TEXT,

    duration_sec INTEGER NOT NULL,

    published_at TEXT NOT NULL,

    url TEXT NOT NULL,

    score INTEGER NOT NULL,

    state TEXT NOT NULL,

    first_seen_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    seen_count INTEGER NOT NULL DEFAULT 1,

    evaluation_json TEXT,

    UNIQUE(episode_id, bvid)
);
```

---

## uploader_trust

```sql
CREATE TABLE uploader_trust (
    anime_id INTEGER NOT NULL,

    uploader_mid INTEGER NOT NULL,

    uploader_name TEXT,

    confirmed_count INTEGER NOT NULL DEFAULT 0,
    rejected_count INTEGER NOT NULL DEFAULT 0,

    manually_trusted INTEGER NOT NULL DEFAULT 0,
    manually_blocked INTEGER NOT NULL DEFAULT 0,

    PRIMARY KEY(anime_id, uploader_mid)
);
```

---

## notification

```sql
CREATE TABLE notification (
    id INTEGER PRIMARY KEY AUTOINCREMENT,

    episode_id INTEGER NOT NULL,

    channel TEXT NOT NULL,

    status TEXT NOT NULL,

    attempts INTEGER NOT NULL DEFAULT 0,

    last_error TEXT,

    created_at TEXT NOT NULL,
    sent_at TEXT,

    UNIQUE(episode_id, channel)
);
```

---

# 29. evaluation_json

每次判断必须可以解释。

例如：

```json
{
  "anime_match": "exact",
  "episode_match": "strong",
  "duration_match": "normal",
  "expected_time_delta_sec": 1800,
  "trusted_uploader": false,
  "negative_keywords": [],
  "score": 75,
  "decision": "pending"
}
```

这样发生误判以后可以知道：

> 为什么系统认为它是真的。

不要只保存：

```text
score = 75
```

---

# 30. Scheduler

每个 Episode 自己维护：

```text
next_check_at
```

而不是给每部番创建 Tokio timer。

主循环：

```rust
loop {
    let due = repository.find_due_episodes(now).await?;

    for episode in due {
        detector.check(&episode).await?;
    }

    sleep(Duration::from_secs(30)).await;
}
```

---

# 31. Polling Policy

根据距离预计更新时间动态调整。

推荐初始配置：

```text
距离预计更新 > 48h
    每 6 小时

48h ~ 12h
    每 2 小时

12h ~ 3h
    每 1 小时

3h before ~ 6h after
    每 15 分钟

6h ~ 24h after
    每 30 分钟

24h ~ 72h after
    每 2 小时

>72h
    每 6 小时
```

以上只是策略默认值。

全部 Configurable。

---

# 32. Jitter

不要固定：

```text
23:00
23:15
23:30
23:45
```

加入：

```text
±10%
```

随机 jitter。

例如：

```text
15 min
```

实际：

```text
13:30 ~ 16:30
```

这是为了避免机械固定节奏，同时也让多个 Anime 不会集中同一秒请求。

---

# 33. 全局 Bilibili Rate Limiter

无论多少番：

```text
Bilibili concurrency = 1
```

请求之间默认至少：

```text
5 sec
```

即：

```text
search A
5 sec
search B
5 sec
detail C
...
```

个人项目不需要并发抓取。

---

# 34. 每轮请求上限

一次 Episode Check：

```text
最多 2 个 search requests
最多 3 个 detail requests
```

正常情况下：

```text
1 search
0~2 detail
```

不要：

```text
搜索 20 个结果
↓
20 个 detail request
```

---

# 35. Global Daily Safety Budget

增加：

```text
max_bilibili_requests_per_day
```

默认：

```text
500
```

达到以后：

```text
暂停非必要请求
记录 warning
```

这个数字不是 Bilibili 的所谓安全线。

它只是本项目自己的安全预算。

---

# 36. 风控处理

## 429

进入：

```text
global backoff
```

例如：

```text
30 min
```

连续出现：

```text
30m
1h
2h
4h
```

---

## 412

视为更严重 Risk Control。

立即：

```text
停止全部 Bilibili 请求
```

例如：

```text
6h
```

之后只做一次 Probe。

仍然 412：

```text
12h
24h
```

不要不断 retry。

---

## 5xx / timeout

指数退避：

```text
5m
15m
1h
```

但不要把整个 Episode 标记失败。

---

# 37. Cookie

第一版优先：

```text
不使用登录账号 Cookie
```

如果公开搜索接口需要：

```text
buvid
WBI
正常 browser headers
```

由 Provider 管理。

禁止把个人主账号 `SESSDATA` 写死在代码或者数据库。

Secret 必须：

```text
environment variable
```

或者独立：

```text
.env
```

且：

```text
chmod 600
```

---

# 38. Bangumi 的定位

Bangumi 是可选 Metadata Provider。

用途：

```text
搜索 Anime
获取规范名称
别名
总集数
Episode 信息
大致放送日期
```

它不作为：

```text
“B站已经有可看视频”
```

的确认依据。

核心系统即使没有 Bangumi，也必须能够运行。

用户可以完全手动添加：

```text
title
aliases
next_episode
expected_at
duration
```

---

# 39. Notification 抽象

```rust
#[async_trait]
trait Notifier {
    async fn notify_release(
        &self,
        event: &ReleaseEvent,
    ) -> Result<()>;
}
```

第一版实现一个：

```text
ServerChanNotifier
```

或者用户最终选定的渠道。

业务层不能依赖 ServerChan。

---

# 40. 推送内容

Confirmed：

```text
📺 Silent Witch EP08 已更新

检测到：2 个独立上传
时间：23:41
最高可信候选：
UP：xxxx
时长：23:42

观看：
https://www.bilibili.com/video/BVxxxx
```

如果 Trusted Uploader：

```text
确认依据：
可信 UP
```

如果 Consensus：

```text
确认依据：
2 个独立 UP 一致
```

让通知本身也可解释。

---

# 41. 推送幂等

必须保证：

```text
EP08
```

无论检测多少次：

```text
每个 channel 最多成功通知一次
```

数据库：

```sql
UNIQUE(episode_id, channel)
```

发送逻辑：

```text
insert notification pending
↓
send
↓
mark sent
```

如果发送失败：

```text
保留 pending
```

以后 retry。

不要因为通知 API timeout 就重新产生 Episode Release。

---

# 42. 下一集推进

EP08 确认并通知后：

```text
EP08 = Notified
```

创建：

```text
EP09
```

预计时间优先：

```text
EP08 expected_at + 7 days
```

如果 Bangumi 可以提供更准确的 Episode 时间：

```text
使用 Bangumi
```

否则默认：

```text
+7 days
```

---

# 43. 连续两集问题

如果突然有人上传：

```text
EP08-09
```

V1：

```text
EpisodeMatch = Ambiguous
NeedsManualReview
```

不要自动：

```text
EP08 Confirmed
EP09 Confirmed
```

因为：

```text
8-9
```

也可能出现在别的含义中。

后续版本单独设计 MultiEpisode。

---

# 44. 视频被删除

可能出现：

```text
23:30 Candidate Confirmed
23:31 push
23:40 视频被删
```

这是无法避免的。

系统的语义是：

> 在某个时刻检测到符合条件的视频。

而不是：

> 保证用户点击时它仍然存在。

后续可以做 availability recheck，但 V1 不做。

---

# 45. 投稿审核延迟

真实流程可能：

```text
UP 投稿
↓
审核
↓
公开
```

系统只能发现：

```text
公开视频搜索能够看到的时刻
```

无法知道实际投稿时间。

这符合项目需求。

---

# 46. 搜索召回率问题

可能存在真正的新一集已经上传，但：

```text
标题故意混淆
没有番名
没有集数
使用奇怪缩写
搜索排序没排到第一页
```

那么系统可能漏报。

V1 接受这一边界。

允许通过 Anime Alias 改善：

```text
aliases
```

但不无限翻页。

只查：

```text
page 1
```

因为项目优先低请求量。

---

# 47. 假视频问题

可能存在 Candidate：

```text
番名正确
EP 正确
时长正确
时间正确
标题正确
```

内部却是假内容。

Metadata-only 系统无法绝对解决。

降低风险的方法：

```text
Trusted uploader
Independent consensus
User feedback
```

不承诺完全消灭。

---

# 48. Consensus 误判问题

两个不同账号可能：

```text
转载同一个假内容
```

所以：

```text
2 uploaders
```

不是数学证明。

以后如果误报仍然明显，可以增加：

```text
3 uploaders
评论信号
用户关注 UP 白名单
更长确认窗口
```

但 V1 先观察实际效果。

---

# 49. UP 主换号问题

一个长期可靠 UP：

```text
被封
换号
停止投稿
```

系统会退化成 Consensus 路径。

这是合理行为。

不要根据：

```text
UP 名字
```

识别 Trust。

必须使用：

```text
mid
```

---

# 50. 用户反馈机制

CLI：

```bash
anime-watcher candidates
```

输出：

```text
BVxxxx
Silent Witch EP08
UP xxxx
23:42
score=82
Pending
```

用户：

```bash
anime-watcher accept BVxxxx
```

或者：

```bash
anime-watcher reject BVxxxx
```

accept：

```text
Candidate → Confirmed
Episode → Confirmed
Uploader trust +1
发送通知（若尚未发送）
```

reject：

```text
Candidate → Rejected
Uploader rejected_count +1
```

---

# 51. 手动 Trust

支持：

```bash
anime-watcher uploader trust <anime-id> <mid>
```

以及：

```bash
anime-watcher uploader block <anime-id> <mid>
```

Blocked Uploader：

```text
所有 candidate 直接 Reject
```

---

# 52. CLI

V1 至少实现：

```bash
anime-watcher run

anime-watcher anime add
anime-watcher anime list
anime-watcher anime show
anime-watcher anime disable
anime-watcher anime enable

anime-watcher candidate list
anime-watcher candidate accept
anime-watcher candidate reject

anime-watcher uploader trust
anime-watcher uploader block

anime-watcher check
anime-watcher check <anime-id>

anime-watcher notification test
```

---

# 53. `anime add`

示例：

```bash
anime-watcher anime add \
  --title "Silent Witch" \
  --alias "沉默魔女" \
  --next-episode 8 \
  --weekday friday \
  --time 23:00 \
  --duration-min 20m \
  --duration-max 28m
```

以后再做交互式输入。

---

# 54. Web UI

V1 不做。

原因：

```text
认证
HTTPS
CSRF
公网攻击面
Session
前端
```

这些与核心问题没有关系。

服务器和域名暂时无需用于管理面板。

先通过：

```text
SSH + CLI
```

管理。

V2 再：

```text
Axum
Caddy
HTTPS
Authentication
```

---

# 55. Observability

使用：

```text
tracing
```

正常日志应该能看到：

```text
checking anime=Silent Witch episode=8

search query="Silent Witch 8"

results=20

candidates_after_filter=2

candidate BVxxx score=72 state=pending

candidate BVyyy score=75 state=pending

consensus distinct_uploaders=2

episode 8 confirmed by consensus

notification sent
```

---

# 56. 不记录敏感 Secret

日志禁止输出：

```text
Cookie
SESSDATA
ServerChan SendKey
Authorization
完整 request headers
```

---

# 57. Metrics

V1 不需要 Prometheus。

数据库或者日志记录以下数据即可：

```text
bilibili_requests
search_requests
detail_requests
rate_limit_events
risk_control_events
candidates_found
candidates_rejected
episodes_confirmed
notifications_sent
```

---

# 58. Config

例如：

```toml
[database]
path = "/var/lib/anime-watcher/anime.db"

[bilibili]
min_request_interval_secs = 5
max_requests_per_day = 500
max_search_requests_per_check = 2
max_detail_requests_per_check = 3

[polling]
far_interval_secs = 21600
near_interval_secs = 3600
release_interval_secs = 900

[confirmation]
minimum_candidate_score = 60
consensus_uploaders = 2
max_duration_delta_secs = 180
max_publish_delta_secs = 7200
candidate_expire_secs = 259200

[notification]
provider = "serverchan"
notify_pending = false
```

Secret 不进 TOML：

```text
SERVERCHAN_SEND_KEY
```

使用环境变量。

---

# 59. 项目目录

建议：

```text
anime-watcher/
├── Cargo.toml
├── migrations/
├── config.example.toml
└── src/
    ├── main.rs
    ├── config.rs
    ├── error.rs
    │
    ├── domain/
    │   ├── anime.rs
    │   ├── episode.rs
    │   ├── candidate.rs
    │   └── notification.rs
    │
    ├── provider/
    │   ├── mod.rs
    │   ├── bilibili.rs
    │   └── bangumi.rs
    │
    ├── detector/
    │   ├── mod.rs
    │   ├── title.rs
    │   ├── episode.rs
    │   ├── evaluator.rs
    │   └── consensus.rs
    │
    ├── scheduler/
    │   └── mod.rs
    │
    ├── repository/
    │   ├── mod.rs
    │   └── sqlite.rs
    │
    ├── notification/
    │   ├── mod.rs
    │   └── serverchan.rs
    │
    └── cli/
        └── mod.rs
```

不要过度拆 crate。

一个 binary crate 足够。

---

# 60. Detector 主流程伪代码

```rust
async fn check_episode(
    episode: Episode,
    anime: Anime,
) -> Result<()> {
    if provider_backoff_active() {
        reschedule_after_backoff(episode);
        return Ok(());
    }

    let query = build_primary_query(&anime, &episode);

    let results = provider.search(&query).await?;

    let mut candidates = Vec::new();

    for result in results {
        if already_processed(&result.bvid, episode.id).await? {
            update_seen(result).await?;
            continue;
        }

        let preliminary = evaluator.evaluate_basic(
            &anime,
            &episode,
            &result,
        );

        if preliminary.hard_reject {
            save_rejected(result, preliminary).await?;
            continue;
        }

        if preliminary.score < DETAIL_THRESHOLD {
            save_pending_or_rejected(result, preliminary).await?;
            continue;
        }

        let detailed = provider.enrich(&result).await?;

        let evaluation = evaluator.evaluate_full(
            &anime,
            &episode,
            &detailed,
        );

        save_candidate(detailed, evaluation).await?;

        candidates.push(...);
    }

    let confirmation =
        confirmer.evaluate_episode(episode.id).await?;

    match confirmation {
        Confirmation::TrustedUploader(candidate) => {
            confirm_episode(candidate).await?;
        }

        Confirmation::Consensus(candidates) => {
            confirm_episode(...).await?;
        }

        Confirmation::None => {
            reschedule_episode(...).await?;
        }
    }

    Ok(())
}
```

---

# 61. Confirm 必须事务化

Episode Confirmation：

```text
Candidate
Episode
Notification
```

之间要保证一致性。

建议事务：

```text
BEGIN

mark candidate confirmed

mark episode confirmed

create notification pending

COMMIT
```

真正 HTTP 推送：

```text
事务外执行
```

成功：

```text
notification = sent
episode = notified
```

失败：

```text
notification 保持 pending
```

这样进程 crash 也不会丢通知。

---

# 62. Scheduler Crash Recovery

程序重启后：

```text
next_check_at <= now
```

立即重新进入调度。

Pending Notification：

```text
status = pending
```

重新发送。

Pending Candidate：

```text
继续参与 Consensus
```

所有核心状态必须在 SQLite。

不要只存在内存。

---

# 63. 测试

这个项目必须重点测试 Detector，而不是 HTTP。

Provider 做 Mock。

---

# 64. Episode Parser 测试

至少覆盖：

```text
EP8
EP08
E08
Episode 8
第8集
第08集
第八集
08

1080P
18
80
2026-08
8月
```

---

# 65. Negative Title 测试

```text
Silent Witch EP08 预告
Silent Witch EP08 解说
Silent Witch EP08 Reaction
Silent Witch EP08 名场面
```

必须明显降低 score 或 Reject。

---

# 66. Duration 测试

目标：

```text
20~28min
```

：

```text
1:30 → reject
5:00 → reject
12:00 → suspicious
23:40 → normal
28:30 → acceptable
45:00 → suspicious
```

---

# 67. Consensus 测试

同一个 UP：

```text
BV1
BV2
BV3

mid=100
```

不得形成 3-vote Consensus。

必须：

```text
distinct MID
```

---

# 68. Duplicate 测试

同一个 BV：

```text
poll 1
poll 2
poll 3
```

只能有：

```text
1 candidate row
```

：

```text
seen_count = 3
```

---

# 69. Notification Idempotency 测试

Episode 8 Confirmed 多次：

```text
只能发送一次成功通知
```

网络 timeout：

```text
Notification pending
```

重启：

```text
retry
```

但：

```text
不能生成第二个 notification record
```

---

# 70. Risk Control 测试

Mock Provider：

```text
412
```

必须：

```text
global backoff
```

之后其它 Anime 也不得继续请求 Provider。

---

# 71. V1 第一阶段实现顺序

不要一次写完整项目。

### Phase 1

实现：

```text
domain model
SQLite
CLI anime add/list
```

---

### Phase 2

实现：

```text
Bilibili Provider
search
normalize result
```

运行：

```bash
anime-watcher check <anime>
```

可以看到候选。

但不推送。

---

### Phase 3

实现：

```text
Title Matcher
Episode Matcher
Duration Filter
Evaluator
```

输出：

```text
candidate + reason + score
```

---

### Phase 4

实现：

```text
Candidate persistence
Consensus
Uploader Trust
```

---

### Phase 5

实现：

```text
Notifier
Notification idempotency
```

---

### Phase 6

实现：

```text
Scheduler
dynamic polling
rate limiter
backoff
systemd
```

---

### Phase 7

可选：

```text
Bangumi metadata integration
```

Bangumi 不应该阻塞 MVP。

---

# 72. MVP 验收标准

第一版达到以下要求即可认为完成。

用户能够：

```text
添加 3~10 部正在追的番
```

并设置：

```text
别名
下一集
预计更新时间
正常视频长度
```

服务：

```text
24/7 systemd 运行
```

能够：

```text
自动搜索公开视频
```

不会：

```text
一搜到 EP08 就直接通知
```

而是至少通过：

```text
Trusted Uploader
OR
Independent Consensus
```

之一。

重复轮询不得：

```text
重复推送
```

Bilibili 风控后不得：

```text
高频重试
```

进程重启不得丢失：

```text
Episode
Candidate
Notification
Backoff
```

状态。

---

# 73. 第一版明确不要做的“聪明功能”

Codex 不要主动增加：

```text
LLM 判断标题
Embedding
向量数据库
Redis
PostgreSQL
Kafka
消息队列
Docker Compose
React 前端
复杂账户系统
Prometheus
Grafana
Kubernetes
浏览器自动化
Playwright
视频下载
评论爬取
弹幕分析
AI 视频分析
```

这些东西对于 V1 都没有必要。

---

# 74. 最重要的工程原则

### 原则一

```text
Search Result != Release
```

---

### 原则二

```text
Candidate != Confirmed Release
```

---

### 原则三

```text
Score != Truth
```

Score 只是辅助。

Confirmation Rule 才决定状态转换。

---

### 原则四

```text
同一个视频重复出现 != Consensus
```

Consensus 必须来自：

```text
不同 uploader_mid
```

---

### 原则五

```text
Bilibili 是不稳定外部依赖
```

所有接口细节只能存在于：

```text
BilibiliProvider
```

---

### 原则六

```text
所有关键状态必须持久化
```

任何业务正确性不得依赖内存状态。

---

### 原则七

```text
False Positive > Detection Delay
```

出现不确定情况：

```text
Pending
```

而不是：

```text
Confirmed
```

---

# 75. 已知无法彻底解决的问题

即使完全按照设计实现，仍然存在：

1. 搜索结果没有真正的视频；
2. 视频标题经过特殊混淆导致搜索不到；
3. Bilibili 搜索排序导致真正视频不在第一页；
4. 多个假视频形成错误 Consensus；
5. Trusted UP 上传了错误内容；
6. 视频发布后立即删除；
7. 番剧临时延期；
8. Episode 编号发生特殊变化；
9. 这周突然连播两集；
10. 总集篇插入正常 Episode 编号；
11. Bilibili API 改版；
12. 云服务器 IP 被风控；
13. Bangumi 时间与实际字幕资源时间差异较大。

这些不是代码 bug，而是问题本身的信息不完备。

系统应该：

```text
显式建模不确定性
```

而不是假装可以完全解决。

---

# 76. 后续 V2 可考虑

只有 V1 实际跑几周以后，再根据真实误报/漏报决定是否增加：

```text
Web UI
Bangumi 自动导入
多推送渠道
评论可信度
UP 主自动学习
异常 Episode
多集视频
Bark
飞书
Telegram
Episode 时间自动学习
更智能的 Query 选择
```

不要提前实现。

---

# 77. 给 Codex 的最终实现要求

实现时优先保证：

```text
正确性
可解释性
可测试性
外部 API 隔离
低请求量
Crash Recovery
```

而不是代码量少。

任何新增设计，如果不是解决上述 V1 Requirement 所必需，应当先不实现。

当需求存在歧义时，遵循：

```text
宁可把 Candidate 保留为 Pending，
也不要自动 Confirm。
```

这是整个项目最高优先级的业务规则。
