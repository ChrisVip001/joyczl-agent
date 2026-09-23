# 更新日志

[English](CHANGELOG.md) | 简体中文

## 0.7.0 — 向当前最优实现对齐

### 一张模型自己维护的待办清单

`todo_write` 维护一张会话级清单（整表替换、版本号单调、最多 20 项、单项 4000 字符、
同时只能一项 `in_progress`），而且它是**每轮注入 system prompt** 的，不放在历史里
—— 能扛住压缩的清单，比扛不住任何东西的清单值钱。省略 `todos` 就是读回全量。
子代理没有它：那张表属于父轮的任务线。

### 轮内工具结果预算

历史有滑窗与 token 预算，轮内没有 —— 而 MCP 工具的输出无人看管。
`JOY_TOOL_RESULT_TOTAL_CHARS` / `JOY_TOOL_RESULT_MAX_CHARS` 会把最大的几条结果换成桩：
完整原文落盘、上下文里只留头尾的**完整行**，跳过自预算的工具、幂等执行，落盘失败
也绝不把成功的调用变成错误。默认阈值刻意给得很大（200k / 30k）。

### 子代理的报告是转述，也可以要结构化

报告现在带一句「这是转述」的声明头；模仿角色前缀或我们控制标记的行会被加反斜杠，
并告诉模型转义了几处。它不判断恶意，也不碰权限检查 —— 那是闸门的事。`delegate_task`
还接受可选的 `result_schema`：结论必须是符合它的 JSON，不合规会带着报错重试一次，
两次都不合规就回落成散文并写明原因。

### 生命周期钩子（`JOY_HOOKS`）

12 个事件（`PreToolUse`/`PostToolUse`/`PostToolUseFailure`/`PermissionRequest`/
`SessionStart`/`SessionEnd`/`Stop`/`StopFailure`/`SubagentStart`/`SubagentStop`/
`PreCompact`/`PostCompact`）可以从 `<home>/hooks.json` 跑你的 shell 命令：exit 2 阻断，
决策 JSON 能改写入参、改写结果、补上下文，`PermissionRequest` 还能替用户拍板。策略
事件超时 fail-closed、观察事件 fail-open；运行中改过的 `hooks.json` 拒绝执行并说明
原因；阻断 `Stop` 让这一轮再跑一次。只做 shell handler、Unix 优先 —— 两条都是刻意的，
都写进了 limitations。

### 记忆不再堆积重复

`facts` 上多了一条唯一索引 `(subject, lower(trim(content)))`，`Facts::add` 返回
`(行, 是否新记)`：重复时把已有的那条交回来，而不是再存一份。提炼只把真正新增的计入
账，`save_note` 会说一句「这条已经记过了」而不是假装又记了一次。迁移 0008 在建索引
之前先清掉历史重复 —— 建不起来的索引会让之后每次写入都报错。

### MCP：挂掉的服务器不再每轮白等一个超时

连接现在带断路器（连续 3 次失败 → 60 秒，按服务器隔离）：开着时调用直接返回说明而
不再敲门，冷到点后放一次探针，成功即清零。会塌成同名的工具（`a`+`b_c` 与
`a_b`+`c`）会加后缀去重并**打印改名**；`ToolRegistry::register` 遇到重名拒绝后到者，
而不是悄悄换掉实现。

### 用实测校准估算

`usage.input_tokens` 过去只是记下来、从不回喂。现在 loop 把**同一个请求**的估算与实测
一起返回，app-server 配对写进 `session_context`（迁移 0007），下一轮的预算按比值修正
（夹在 0.5–2.0，一次异常请求带不偏它）。

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

### 技能有了策略字段，记忆有了类别

技能现在可以声明 `allow-model-invocation: false`（「别不小心把我叫出来」）与
`dependencies: a, b`。声明了不隐式触发的、以及依赖缺失的技能，都不会因关键词重合
而载入；消息里的 `$技能名` 强制把正文放进去（并且会从模型看到的消息里剥掉）。
引用了不存在的东西是一句提示，不是错误。

事实带上了 `kind`（`user` / `feedback` / `project` / `reference` / `fact`），由提炼
模型给出、在写入口收敛，并通过 `memory/search`、`memory/list` 与协议里的 `Fact`
暴露出来。提炼失败现在按指数退避（1 分钟翻倍到 1 小时），而不是每轮重试同一批
坏行。

### 交互式批准（`JOY_APPROVAL=on-request`）

放行表没匹配上的命令，现在可以是「问一句」而不是直接拒绝。这一轮发
`approvalRequested`（工具、命令原文、为什么、时限）然后等；`approval/respond`
回答它；终端与驾驶舱都实现了回答那一侧。**沉默就是拒绝**：超时、答得太晚、
连接断了、根本没有界面，结局都是不跑。只有放行表那一关可以商量 —— 硬拒名单与
沙箱批不掉。「记住」把原样那条命令写进 `settings.json`，下次启动生效。

### 子代理（`JOY_DELEGATE`）

`delegate_task` 把一件自成一体的活交给子代理，在一个干净上下文里（空历史、不带
检索、不带技能）跑完，只把结论带回来。它拿到的是父轮工具**去掉 `delegate_task`**
的副本 —— 再派一层不是被拒绝，而是不可能；它也不能向你提问；答案的预算上限是
5 轮（硬上限 10）与 2048 token。结论后面附上它用过的工具（含失败的调用），父轮
因此能判断这份结论是不是建立在失败之上。默认关。

### 超长命令输出会留下来，不只是截断

超过 8000 字符的部分照旧不进模型看到的内容（上下文要保住），但完整原文会写进
`<home>/spill/<日期>/…` 并把路径报出来，随时可以回查。`spill/` 保留 7 天、启动
时打扫；写失败则退回纯截断。

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
