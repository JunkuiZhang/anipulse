# AniPulse V1 多阶段实施方案

本方案以 `doc/thoughts.md` 为需求基线，优先保证低误报、可解释、低请求量、可恢复和 Linux 长期运行。V1 的管理界面仅为 CLI；Bangumi 和 Web UI 不进入 MVP。

## 阶段 1：可持久化的管理内核

交付：配置加载、SQLite migration、Anime/Alias/Episode 领域模型，以及 `anime add/list/show/enable/disable`。

验收：添加番剧时自动创建目标 Episode；重启后所有状态仍存在；配置文件不存在时可用安全默认值启动 CLI。

## 阶段 2：可测试的本地判定器

交付：标题规范化、番名匹配、整数 Episode 解析、时长/负面关键词/发布时间评分和结构化 `evaluation_json`。

验收：`EP08`、`第八集` 等正常识别；`1080P`、`8月`、`8-9` 不被错误当作单集；任何自动确认都不能只依赖分数。

## 阶段 3：候选、信任与确认状态机

交付：Candidate 去重与 `seen_count`、每番剧独立的 Uploader Trust、Blocked 规则、Trusted Uploader 和不同 MID Consensus 两条自动确认路径，以及手工 accept/reject。

验收：同一 BV 或同一 MID 重复出现不能形成共识；确认事务同时写入 Episode、Candidate 和 pending Notification；失败/不确定结果保持 Pending。

## 阶段 4：低频 Bilibili Provider

交付：Provider trait、WBI 搜索签名、公开视频 metadata 映射、详情补全、全局串行请求间隔、每日安全预算，以及 429/412/临时错误的持久化退避。

验收：业务层不引用 Bilibili JSON/URL；单次检查最多 2 次搜索和 3 次详情；遇到风控后其他番剧也停止请求。

## 阶段 5：通知与无人值守调度

交付：飞书自建应用机器人私聊卡片（群 Webhook 兼容）、访问令牌缓存、通知幂等与重试、动态 `next_check_at`、jitter、下一集推进、`run` 常驻循环、systemd unit。

验收：每 Episode/Channel 最多一条通知记录；发送失败保持 pending；重启可恢复 due Episode、pending Candidate、pending Notification 和 Provider backoff。

## 阶段 6：部署与质量门槛

交付：示例配置、飞书应用配置与 Linux 安装说明、结构化日志、单元/SQLite/通知 HTTP 集成测试。

验收：`cargo fmt --check`、`cargo test`、`cargo clippy -- -D warnings` 通过；可通过 SSH + CLI 完成添加、检查、纠错和信任管理。

## 阶段 7：自动排期元数据

交付：`anime add --auto-schedule`、Bangumi ID 消歧、bangumi-data 别名与 `broadcast` 导入、Bangumi 章节日期校准、每日后台同步和失败退避。

验收：唯一精确标题可自动补全排期；同名季度必须显式指定 ID；同步失败保留旧排期；通知成功创建下一集后立即触发元数据校准。

## 阶段 8：安全删除与网页前置边界

交付：`anime remove ID --yes`、默认拒绝未确认删除、事务化外键级联、关联数据回归测试和服务器操作文档。

验收：错误 ID 不改变数据；删除后相关 Alias、Episode、Candidate、Notification 和 Uploader Trust 全部消失；CLI 明确提示操作不可撤销。

## 后续阶段（不阻塞 V1）

带鉴权网页管理端已进入后续路线，详细架构、鉴权威胁模型、数据表、路由、systemd/HTTPS 部署和七阶段验收标准见 [`web-management-plan.md`](web-management-plan.md)。它将保持 CLI 作为恢复入口，并让网页进程与监控进程隔离。不会加入下载、评论/弹幕分析、浏览器自动化、LLM 判定或无必要的复杂基础设施。
