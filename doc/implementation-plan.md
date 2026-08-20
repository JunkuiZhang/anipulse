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

交付：ServerChan 通知、通知幂等与重试、动态 `next_check_at`、jitter、下一集推进、`run` 常驻循环、systemd unit。

验收：每 Episode/Channel 最多一条通知记录；发送失败保持 pending；重启可恢复 due Episode、pending Candidate、pending Notification 和 Provider backoff。

## 阶段 6：部署与质量门槛

交付：示例配置、Linux 安装说明、结构化日志、单元/SQLite 集成测试。

验收：`cargo fmt --check`、`cargo test`、`cargo clippy -- -D warnings` 通过；可通过 SSH + CLI 完成添加、检查、纠错和信任管理。

## 后续阶段（不阻塞 V1）

实际运行数周并收集误报/漏报后，再评估 Bangumi 元数据、异常 Episode、多渠道通知和 Web UI。不会提前加入下载、评论/弹幕分析、浏览器自动化、LLM 判定或复杂基础设施。
