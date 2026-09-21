# 更新日志

[English](CHANGELOG.md) | 简体中文

## 0.6.0 — 护栏、预算，以及一个离线的沙箱

### 配置在启动期校验

越界的值过去会静默变成默认值。现在 `Settings::validate` 在环境变量与
`settings.json` 合并之后跑一次，非法值直接终止并在消息里点名是哪个变量。
边界表（`joyczl-config::BOUNDS`）同时服务于启动路径与 `config/write`。

### 工具参数按 schema 校验

每个工具本来就声明了 `input_schema`；现在它在注册时被编译，并在 handler
之前强制执行。于是非法调用回来的是一句
`Error: 参数不符合 … 的 schema —— /subject：42 is not of type "string"`，
而不是各工具手写的提示；而且校验没过时 handler 根本没跑，不留半个副作用。

### 循环护栏

模型卡在同一个调用（连续三次完全相同）或在几个调用之间来回（`A,B,A,B`）时
现在会被告知，字节级重复的长结果会被换成引用桩。交替循环需要单独判定 ——
它会把「连续相同」的计数重置 —— 所以两种形状都抓。`TurnMeta.guard_hits`
让「这一轮卡过」在 trace 与驾驶舱里都看得见。

### 上下文按 token 做预算

`ProviderInfo.context_window` 加 tiktoken 估算决定什么时候压缩：轮数从此
是**上限**，token 才是闸门。provider 报「上下文超了」会被认出来
（`ProviderError::ContextOverflow`），这一轮会压缩后**重试一次**。
`JOY_CONTEXT_WINDOW` / `JOY_COMPACT_THRESHOLD` 可以调；校验会拒绝
`JOY_MAX_TOKENS >= JOY_CONTEXT_WINDOW` 这种答案放不下的配置。

### 限流与临时故障会重试，而且看得见

429/5xx/网络抖动会指数退避重试（单次等待 500ms 起、上限 8s，总预算 30s，
`JOY_LLM_RETRIES` 默认 2，`0` 关闭）。每次重试都发一条 `Retry` 通知并记进
`TurnMeta.retries`，于是那段等待是**有解释的**而不是莫名其妙的；REPL 打一行
小字，驾驶舱显示一枚标记。服务端给了 `Retry-After` 就按它等（受上限约束）。
不做 provider failover —— 那是另一个决定。

### 行为变更：沙箱内的命令默认断网

`run_command` 现在默认**没有网络** —— seatbelt 加 `(deny network*)`，
bubblewrap 加 `--unshare-net`。需要下载东西的命令（比如会拉 crate 的
`cargo test`）必须设 `JOY_EXEC_NETWORK=1`。额外可写的目录用
`JOY_EXEC_WRITABLE_ROOTS` 放开（冒号分隔，每条必须是已存在的绝对目录），
构建缓存是典型用例。启动那一行会把两者都报出来。

## 0.5.0 — 本地、沙箱、定时

- 本地推理：`ollama` provider —— 不要 key、不要网络，对话不出这台机器。
  LM Studio / vLLM 用 `JOY_BASE_URL` + `JOY_MODEL` 指过去
- `joy mcp serve`：Joy 成为一台 MCP 服务器，暴露五个记忆工具，这台机器上的
  其他 agent 共享同一份事实（**只暴露记忆**，不暴露任何「替别人动手」的能力）
- 沙箱执行：`run_command` 挂在 `JOY_EXEC` 后面，默认关，三道闸门 ——
  不可配置的硬拒名单、放行表（空 = 全部拒绝）、macOS `sandbox-exec` /
  Linux `bubblewrap`（写权限限制在工作目录、Joy 的 home 与临时目录）。
  沙箱不可用就拒绝执行，绝不在沙箱之外跑
- 上下文压缩：被滑窗挤出去的轮次折进按会话的滚动摘要（存 state.db，只往前滚、
  不重算全史）；摘要模型罢工时退化为确定性摘录
- `joy schedule`：定时任务可声明在技能 frontmatter（`schedule: 0 8 * * 1-5`）
  或 `schedules.json` 里，五字段 cron，同一分钟最多触发一次，结果写进 outbox
- 混合检索（`JOY_EMBEDDINGS`，关键词与向量两条腿按名次 RRF 融合）+
  `joy memory reindex` 给开关打开之前的事实补向量
- consolidation 过滤临时陈述，不把它们当成事实存档
- `joy skill update`：按索引更新 —— 先校验、再暂存、备份、原子替换，从不降级
- 分发：Dockerfile（多阶段、非 root、装了 bubblewrap）与 Homebrew formula 模板
- `docs/limitations.zh.md`：已知边界清单（刻意的与未做的），每条都指出下一步
  该打开哪个文件

## 0.4.0 — 功能面补齐

- 协议：`turn/interrupt`（取消令牌 + 竞速收兵）、`config/write`
  （settings.json 持久化 + 热生效）、`model/list` 全部实现；
  `ToolStarted` 通知、`TurnMeta.usage` / `meta.interrupted` 入库
- CLI：终端 REPL（`joy` 裸跑）、`joy gather` 晨报、`joy mcp login`、
  `joy skill export/install/list`
- 工具：`search_web`、`create_event`/`list_events`（幂等 + ICS +
  Apple Calendar 可选同步）、`send_message`（outbox 草稿）、
  `manage_memory`、`create_skill`
- 记忆：Skills 过程记忆（渐进披露 + 关键词触发）、MEMORY.md 每轮镜像
- 观测：trace 落 `traces/<日期>.jsonl`、usage 账本 `usage.jsonl`
- 图：gather 晨报工作流（四路并行、只提议不行动、失败开放）
- MCP：浏览器 OAuth（发现 / 动态注册 / PKCE / 回调 / token 落盘 /
  刷新），stdio + HTTP 双传输
- 评测：`joyczl-eval`——确定性 eval（13 场景）+ release gate（`just
  check` 尾部）+ judge
- 文档：architecture / configuration / protocol / testing /
  operations / skills / CONTRIBUTING / SECURITY

## 0.3.0 — 驾驶舱与网关

- `joyczl-ops`：axum + SSE 驾驶舱后端；`@joy/dashboard` 前端
- 网关：Telegram、Discord（WebSocket + 心跳重连）、微信（三种加解密
  模式 + 4 秒竞速 + 客服消息）、飞书（长连接 + pbbp2 帧 + 分片重组）
- 会话：`session/*` 协议与历史翻页

## 0.2.0 — loop 与编排

- `joyczl-provider`：11 家厂商、Anthropic/OpenAI 双 wire format、SSE 流式
- `joyczl-loop`：observe → reason → act → repeat，工具错误文本化
- `joyczl-memory`：检索门（失败开放）+ consolidation
- `joyczl-tools`：内置工具注册表；`joyczl-graph`：波次 DAG 引擎 +
  triage 前门；`joyczl-mcp`：stdio + HTTP 传输

## 0.1.0 — 地基

- `joyczl-protocol`：单一事实来源 + TS/JSON Schema/pydantic 生成管线
- `joyczl-state`：SQLite + FTS5(trigram) + 迁移
- `joyczl-app-server`：JSON-RPC over stdio；`joy` CLI
