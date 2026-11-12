# 协议参考（app-server v2）

[English](protocol.md) | 简体中文

`joy app-server` 在 stdin/stdout 上说**换行分隔的 JSON-RPC 2.0**：一行一个
请求，应答与通知混序写出（通知先于应答）。所有类型定义在
`joyczl-protocol/src/protocol/v2.rs`（130 个类型，含 JSON-RPC 信封），并生成 TypeScript
（`schema/typescript/v2/`）与 Python pydantic（`sdk/python/generated/`）。
**改协议必须跑 `just write-app-server-schema`。**

## 方法一览（41 个）

| 方法 | 请求 → 应答 | 说明 |
|---|---|---|
| `turn/start` | `TurnStartParams` → `TurnStartResponse` | 跑一轮对话；应答在全部通知之后发出 |
| `turn/interrupt` | `TurnInterruptParams` → `TurnInterruptResponse` | 打断在跑的回合；`interrupted:false` = 已经跑完 |
| `turn/steer` | `TurnSteerParams` → `TurnSteerResponse` | 往**正在跑**的回合补一句：下一个轮次边界生效，已跑完的工具不重跑 |
| `turn/follow-up` | `TurnFollowUpParams` → `TurnFollowUpResponse` | 回合结束后由服务端起下一轮（会话级订阅） |
| `turn/list` | `TurnListParams` → `TurnListResponse` | 正在跑的回合，可按 `sessionId` 过滤 |
| `approval/respond` | `ApprovalRespondParams` → `ApprovalRespondResponse` | 回答一次「要不要执行」；太晚送达时 `accepted: false` |
| `approval/log` | `ApprovalLogParams` → `ApprovalLogResponse` | 审批审计（迁移 0011）：最近 100 条，四态 `approved` / `denied` / `timeout` / `voided` |
| `todo/view` | `SessionViewParams` → `TodoListView` | 当前的待办清单；模型没写过就不渲染 |
| `task/create` | `TaskCreateParams` → `TaskEnvelope` | 建任务：会话可空，带验收条件、预算、工作目录 |
| `task/list` | — → `TaskListResponse` | 全部任务，新的在前 |
| `task/view` | `SessionViewParams` → `TaskViewResponse` | 会话当前的任务卡 |
| `task/get` | `TaskGetParams` → `TaskEnvelope` | 单个任务：验收、预算、花费、执行次数 |
| `task/runs` | `TaskGetParams` → `TaskRunsResponse` | 执行记录：每次的开始、结束与停下的原因 |
| `task/artifacts` | `TaskGetParams` → `TaskArtifactsResponse` | 交付物清单（真实写出来的文件） |
| `task/suspend` | `TaskActionParams` → `TaskEnvelope` | 暂停：保留断点，不丢已完成的工具 |
| `task/resume` | `TaskActionParams` → `TaskEnvelope` | 继续：从断点接着跑 |
| `task/cancel` | `TaskActionParams` → `TaskEnvelope` | 取消（终态，不再自动续） |
| `inbox/list` | — → `InboxListView` | 连接器收件箱：最新一封 + 还有多少等人处理 |
| `inbox/summarize` | `InboxSummarizeParams` → `InboxSummarizeResponse` | 把收件箱整理成一轮任务 |
| `session/new` | `SessionNewParams` → `SessionNewResponse` | 建会话：落库里的一行，可带名字与工作目录 |
| `session/list` | `SessionListParams` → `SessionListResponse` | 会话列表（游标分页） |
| `session/get` | `SessionGetParams` → `SessionGetResponse` | 单个会话的元数据 |
| `session/update` | `SessionUpdateParams` → `SessionUpdateResponse` | 改名 / 改工作目录（`null` = 不动，`""` = 清空） |
| `session/messages` | `SessionMessagesParams` → `SessionMessagesResponse` | 一个会话的消息，最新在前，往上翻页 |
| `memory/search` | `MemorySearchParams` → `MemorySearchResponse` | facts + episodes 关键词检索 |
| `memory/list` | `MemoryListParams` → `MemoryListResponse` | 最近事实（游标分页） |
| `memory/list-episodes` | `MemoryListEpisodesParams` → `MemoryListEpisodesResponse` | 最近情景 |
| `memory/remember` | `MemoryRememberParams` → `MemoryRememberResponse` | 写一条事实 |
| `memory/forget` | `MemoryForgetParams` → `MemoryForgetResponse` | 按主题删除 |
| `config/read` | `ConfigReadParams` → `ConfigReadResponse` | 完整配置视图 |
| `config/write` | `ConfigWriteParams` → `ConfigWriteResponse` | 应用补丁并热生效（落 settings.json） |
| `model/list` | `ModelListParams` → `ModelListResponse` | 12 家 provider 的模型目录 |
| `model/profiles` | `ModelProfilesParams` → `ModelProfilesResponse` | 已配置的多套模型 + 当前那套（**不含密钥**，只有「key 在哪」的指针） |
| `model/save` | `ModelSaveParams` → `ModelSaveResponse` | 新增/改一套并切成当前；只带 `id` + `makeActive` 就是切换 |
| `model/remove` | `ModelRemoveParams` → `ModelRemoveResponse` | 删一套，连它的凭据一起 |
| `model/routes` | — → `ModelRoutesResponse` | 路由表：`fixed` / `auto` + 三行「档位 → 配置 id」（没配的档位是 `null`，表示跟随缺省链） |
| `model/routes/save` | `ModelRoutesSaveParams` → `ModelRoutesResponse` | 整表替换；档位名与 profile id 都在写入前校验，拼错直接报错 |
| `usage/summary` | `UsageSummaryParams` → `UsageSummaryResponse` | 用量台账（迁移 0010）：per-model 聚合 + 总数。**只记账不强制** —— 没有配额、没有限速 |
| `schedule/list` | `ScheduleListParams` → `ScheduleListResponse` | 调度状态快照（迁移 0012）：每任务的 `last_fired` / `claimed` / `catch_up` / `delivery`。只读 —— 写入权在调度器 |
| `memory/ledger` | `MemoryLedgerParams` → `MemoryLedgerResponse` | 一个会话的账本，最新在前；带 `turnId` 时连同那一轮的条目一起给。只读 |
| `memory/index` | — → `MemoryIndexResponse` | bundle 的目录（`index.md`）以及每页一句话摘要。只读 |
| `memory/page` | `MemoryPageParams` → `MemoryPageResponse` | 一页的正文与 `sources`；没有这一页时是 `page: null` |
| `context/usage` | `ContextUsageParams` → `ContextUsageResponse` | 上一轮的六段占用明细；`usage: null` = 还没测量过 |
| `skill/stats` | — → `SkillStatsResponse` | 汇总 `<home>/skill-hits.jsonl`。只读 |
| `session/compact` | `SessionCompactParams` → `SessionCompactResponse` | **现在**把较早轮次折进滚动摘要，最近两轮留在上下文里。只有人能发起 —— 没有任何工具能走到它 |

约定：方法名 `<resource>/<method>`（resource 单数）；载荷命名
`*Params` / `*Response` / `*Notification`；wire 字段 camelCase；整数一律
`i32`（`i64` 会被 ts-rs 映射成 `bigint`，与 `JSON.parse` 的 `number`
运行时不符）；列表方法一律 `cursor`/`limit` → `data` + `next_cursor`。

### 目标循环的用法

目标**不是一个独立对象**：它写在任务行的 `goalCondition` 上（0.17 起），所以协议里没有
`goal/set` / `goal/view` —— 人要设目标就走 `/goal` 或 `task/create {goalCondition}`，
循环读同一行。唯一的协议面是通知 `GoalRound`（每判一次发一条）：

```
you>  /goal 让测试全绿
       → task/create  {sessionId: "…", title: "让测试全绿", goalCondition: "让测试全绿"}
… 一轮跑完，判断器说不算达成 …
       ← GoalRound {turnId, round: 1, maxRounds: 5, status: "continuing", reason: "还有两条红"}
… 又跑一轮 …
       ← GoalRound {turnId, round: 2, maxRounds: 5, status: "satisfied", reason: "全绿了"}
```

`TurnMeta.goalStatus` / `goalRounds` 把出口与轮次也带进这一轮的元数据。
`TurnMeta.routeReason`（可选）说明这一轮的模型是怎么选的：`auto:reasoning → ds-r1`
或 `fallback: …`；没发生选择时为空。

## turn/start 的通知序列

```
turnStarted → gateDecided → retry* → textDelta* → toolStarted → toolCompleted*
            → consolidationCompleted? → turnCompleted →（应答）
```

重试可能出现在任何一次模型调用之前（门、loop、压缩摘要都算），所以它在序列里
是**可插入**的，位置不固定。

* `gateDecided`：检索门判定（retrieve/skip + 理由 + 检索词）。
* `retry`：一次重试（第几次、为什么、等多久）。**每次必发** —— 重试绝不
  静默；`meta.retries` 记着总数。
* `approvalRequested`：有一条命令在等人批准（工具、命令原文、拒因、时限）。
  收到之后要调 `approval/respond`，那一轮才继续；**不回答就是拒绝**。
* `textDelta`：流式文本增量；仅 `stream:true` 时产生。
* `cwd`（可选）：本轮工作目录。必须是已存在的绝对路径（fail-closed）；`run_command` 的回退链是 `args.cwd > cwd > 进程 cwd`，`read_file`/`list_dir` 的相对路径也按它解析（会话级 working-directory）。子代理继承父轮的 cwd。
* `toolStarted` / `toolCompleted`：工具开始与完成（含耗时与状态）。
* `turnCompleted`：`reply`、`iterations`、`usage`、`meta`（gate / graph /
  tools / model / provider / latency / interrupted / guardHits / retries）。
  meta 随对话落库。

### `Fact.kind`

记忆的类别：`user` / `feedback` / `project` / `reference` / `fact`（兜底）。
`memory/search`、`memory/list` 的应答与 `turn/*` 的检索上下文都带着它；
`memory/remember` 可以传，写歪了一律收敛成 `fact`。

## 错误码

| 码 | 含义 |
|---|---|
| -32700 / -32600 / -32601 / -32602 / -32603 | 标准 JSON-RPC：解析 / 请求 / 方法不存在 / 参数 / 内部错误 |
| `-32000` PROVIDER_ERROR | 模型侧失败（缺 key、401、限流……），附「怎么办」 |
| `-32001` TOOL_ERROR | 工具执行失败（错误同时作为文本回给模型，不中断 turn） |
| `-32002` NOT_IMPLEMENTED | 协议定义了但尚未实现（当前 45 个方法已全部实现） |

## 客户端

* TypeScript：`joy-ts/packages/client`（`StdioTransport` + `JoyClient`，
  通知按判别式 `type` 分发）。
* Python：`sdk/python/joyczl_agent`（`JoyClient.connect()` 拉起子进程，
  pydantic 运行时校验；通知判别用 `n.type`，不要用 isinstance）。
* 终端 REPL 与 dashboard 走进程内/HTTP 翻译的同一套方法面。
