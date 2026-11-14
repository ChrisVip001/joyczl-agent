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
| `turn/operations` | `TurnOperationsParams` → `TaskOperationsResponse` | 按会话查询普通聊天的未知操作，可限定回合 |
| `turn/reconcile` | `TurnReconcileParams` → `TaskOperationsResponse` | 人工记录确切操作的证据，不执行或继续 |
| `turn/start` | `TurnStartParams` → `TurnStartResponse` | 跑一轮对话；应答在全部通知之后发出 |
| `turn/interrupt` | `TurnInterruptParams` → `TurnInterruptResponse` | 打断在跑的回合；`interrupted:false` = 已经跑完 |
| `turn/steer` | `TurnSteerParams` → `TurnSteerResponse` | 往**正在跑**的回合补一句：下一个轮次边界生效，已跑完的工具不重跑 |
| `turn/follow-up` | `TurnFollowUpParams` → `TurnFollowUpResponse` | 回合结束后由服务端起下一轮（会话级订阅） |
| `turn/list` | `TurnListParams` → `TurnListResponse` | 正在跑的回合，可按 `sessionId` 过滤 |
| `approval/respond` | `ApprovalRespondParams` → `ApprovalRespondResponse` | 回答一次「要不要执行」；太晚送达时 `accepted: false` |
| `approval/log` | `ApprovalLogParams` → `ApprovalLogResponse` | 审批审计（迁移 0011）：最近 100 条，四态 `approved` / `denied` / `timeout` / `voided` |
| `todo/view` | `SessionViewParams` → `TodoListView` | 当前的待办清单；模型没写过就不渲染 |
| `task/create` | `TaskCreateParams` → `TaskEnvelope` | 建任务：会话可空，带验收条件、预算、工作目录 |
| `task/start` | `TaskStartParams` → `TaskEnvelope` | 人工授权入队，在独立工作会话中执行 |
| `task/wait` | `TaskWaitParams` → `TaskEnvelope` | 开始前声明一个不可修改的外部输入条件 |
| `task/signal` | `TaskSignalParams` → `TaskSignalResponse` | 经获授权宿主提交一个关联的输入事件 |
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
| `memory/setValidity` | `MemoryValidityParams` → `MemoryValidityResponse` | 人工替换事实有效区间 |
| `memory/provenance` | `MemoryProvenanceParams` → `MemoryProvenanceResponse` | 查看有效期与提炼上下文 |
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
* `cwd`（可选）：本轮工作目录。必须是已存在的绝对路径（fail-closed）；`run_command` 的回退链是 `args.cwd > cwd > 进程 cwd`，`read_file`/`list_dir`/`glob`/`grep`/`delete_file`/`move_file` 的相对路径也按它解析（会话级 working-directory）。子代理继承父轮的 cwd。
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

## 独立任务执行

`task/create` 只记录任务。人可以调用 `task/start {taskId, message}` 或点击驾驶舱的「开始任务」，授权首次执行。message 是实际指令，驾驶舱使用任务标题与目标。SQLite 持久队列由一个 worker 消费。创建本身不启动工作，模型没有开始任务的工具。

`AgentTaskView.executionSessionId` 指向独立执行记录；`sessionId` 仍是原交流会话。`dispatchState` 为 pending / claimed / finished / interrupted，只描述首次派发，不代表任务最终结果。暂停与等待作业仍使用原任务状态。原聊天可独立继续，并携带有上限、限定本会话的任务进度；不复制工作对话。没有验收条件的产出在进度中标为尚未核验。

每个任务只接受一次首次派发，重复开始会拒绝。待执行记录跨重启保留；已领取工作由 checkpoint 恢复负责，没有 checkpoint 的领取进入待核对，不重新提交。排队期间暂停后必须人工恢复，取消则不执行。`POST /api/task` 支持 `action: "start"` 与同样参数；TypeScript、Python 客户端提供 `TASK_START`。

这是本地 worker，不是全天在线服务或分布式租约系统；app-server 运行且模型已配置时才工作。不自动接收外部回调，不授予额外能力。其他手动回合沿用原并发规则；多个操作修改同一外部资源仍需协调。

## 持久化外部输入

任务开始前，用户或获授权的宿主可调用 `task/wait {taskId, source, eventKey, description, expiresAt?}` 声明一个不可修改的输入条件。每个任务限一个条件；`expiresAt` 是 RFC 3339 格式的输入到达期限，按时接收的数据可以在期限后处理。任务可保留草稿并暂停等待，不消耗额外模型轮次，陪伴聊天保持独立。

通过 `task/signal {conditionId, source, eventKey, eventId, data}` 提交不超过 16 KiB 的 JSON 数据。来源与事件键必须匹配；相同来源、事件 ID、目标和数据的重复提交返回原事件确认，修改目标或内容会被拒绝。这些字段仅用于关联，不提供发送者身份认证；必须使用现有的鉴权宿主传输，不可直接暴露为公开 webhook 或模型工具。

事件提交后仅唤醒准确绑定的等待执行一次，重启后也可恢复。检查点保留完整会话和草稿；输入明确作为不可信数据，不增加权限。接收输入不代表完成，恢复后仍须满足原预算和验收条件。人工暂停和取消优先；人工暂停期间到达的数据会保存，直到明确调用 `task/resume`。继续请求等待旧执行收尾后，将人工授权、任务激活和检查点重新打开作为同一事务提交。

## 操作日志与人工核对

`task/operations {taskId}` 返回持久化操作身份、参数和结果摘要、执行意图或已观察结果、人工证据及当前检查点摘要。本地操作键不是服务端幂等键。自动重放策略为 `never`；存在意图只说明操作可能执行过，也可能尚未执行。

人工停止任务并检查真实外部状态后，可调用 `task/reconcile {taskId, operationId, checkpointDigest, resolution, evidence}`。结果限 `reconciled_success`、`reconciled_failure` 或 `not_executed`，证据为 1–16384 字节。必须匹配最新的已暂停执行，以及确切的未决调用和参数。证据与明确标记的工具结果回执和检查点原子提交。相同证据可重复确认，冲突结算会被拒绝。

核对不会执行工具、清除人工暂停或恢复任务。全部未决调用核对后仍须明确调用 `task/resume`。人工确认不是实际观察到的工具成功返回，不满足 `toolsOk`；原产物验收和预算仍适用。近期操作证据会独立于滚动摘要重建到持久化交接中。这些方法是鉴权的人工或宿主控制入口，不是模型工具。

`ToolOperationView.origin` 区分与执行检查点共同提交的意图（`checkpoint`）和从未变化、已停止的旧检查点重建的未决元数据（`recovered_checkpoint`）。两者都不证明已经发送。人工暂停会将任务围栏、持久等待取消及可恢复检查点暂停作为同一事务提交；提交失败保留原等待。

## 版本化验收证据

`CheckView.artifactSha256`（如有）标识文件验收实际读取的确切字节。文件存在检查流式读取普通文件；子串检查在同一次稳定文本读取上匹配并计算摘要。读取期间变化或无法读取的文件不能通过检查。历史摘要不表示该路径现在仍包含相同内容。

非空验收结果现与不可修改的 task/run/round 验收回执（迁移 0038）、文件清单及当前检查投影共同提交。证据提交失败会阻止完成并等待人工确认。内容变化生成关联的新回执，包括 A→B→A 的回退；紧邻的相同测量复用最新回执。回执完整性覆盖测量、观察时间及前序 ID。成功任务的完成回执绑定对应确切 run 的验收 ID。不生成归档文件副本。

仅由模型判断目标达成可以停止目标循环，但不构成独立验收。没有验收条件时，即使存在目标条件，完成回执和聊天进度仍使用 `produced_unverified`，驾驶舱也相应标明尚未核验。

## 记忆有效期与提炼来源

人工客户端通过 `memory/setValidity`（`MemoryValidityParams` → `MemoryValidityResponse`）
替换事实的完整有效区间。传入 `factId`、`validFrom` 和 `validUntil`；时间使用 RFC3339，
null 清除对应边界，结束必须晚于开始。开始包含、结束不包含。这些控制不作为模型工具。
`memory/provenance`（`MemoryProvenanceParams` → `MemoryProvenanceResponse`）读取有效区间和
已记录的提炼上下文；保留事实 ID 时也可查看已失效事实的这些元数据。

迁移 0039 保存实际领取的聊天行 ID，以及角色/内容的 SHA-256，作为提炼输入上下文。
这标识模型看到的整批输入，不证明哪句话支持该事实。同一上下文重试不会增加确认次数。
更正清除旧声明的上下文；删除级联删除上下文。旧事实可能没有这些记录。

迁移 0040 从关键词、向量和最近事实召回中排除失效事实。有时间边界的事实即便当前有效，
也不写入无法自行过期的静态镜像。提示词目录与 `read_memory_page` 在读取时检查数据库来源；
任一来源已删除、更正或存在时间边界，整份快照退出自动召回。没有事实引用的人工页面保持独立。
现有文件、聊天历史、事件摘要、断点与备份不会被擦除或追溯脱敏。有效期过滤针对事实，不覆盖事件摘要。
迁移 0041 为后台提炼增加绑定原始消息的临时观察。提炼合同将长期 `facts` 与可选的
`states` 分开。每个状态带有确切 `sourceRef`、4–400 字符的用户原文片段、[0,1] 范围内的
模型自报置信度和 1–24 小时有效期。引用必须属于本次领取的输入，且与现存用户消息的
SHA-256 一致；助手文本或不匹配的引文被拒绝。到期时间从原消息计算，不从提炼或重试
时刻计算。来源已过期则不产生临时事实。事实、有效期与评估记录原子提交。

`memory/provenance` 另返回可选的 `validityOrigin`（`human` 或 `consolidation`）以及每次观察
可选的 `assessmentJson`，包含原文、来源、置信度与按来源时间计算的有效期。置信度是
未经校准的模型判断；引文匹配不证明其解释正确，也不证明某人的当前情绪。相同观察
重试不会续期。人工内容/有效期修改和删除优先；历史无期限声明不会自动重新分类。
这些元数据不增加模型控制方法。

召回将有期限声明标为临时，将事件摘要标为历史总结。事实有效期不擦除原始历史，也不让
事件摘要过期。模型分类错误、语义改写、显式笔记工具及历史无期限情绪不在自动 TTL 保证内。
提炼结构不合法时保留来源记录并退避重试，不静默标为已处理。提炼指令与 JSON 交流数据
使用不同的消息角色。

迁移 0042 区分已领取批次与已完成记录。独占运行时启动只释放有持久领取标记的未完成批次。
即使新聊天到来或新任务阈值改变，重试仍保留原始来源集合；新批次最多 4,096 行。退避更新
在同一事务中提交。按来源上下文提交的回执防止相同批次重复插入或替换事件摘要；人工删除
摘要后回执作为墓碑保留。已完成记录与没有领取元数据的旧记录不会重新打开。升级无法识别
迁移前遗留、但没有标记的领取。

记忆 worker 存活时保留数据库执行所有权。正常关闭先短暂等待，再取消并等待停滞的提炼调用
退出；丢弃 worker 会中止其循环。恢复提炼可能再次调用只读模型并产生用量，不会重放任务工具。
未收到的供应商用量仍不能精确计入任务账目。

## 结构化文档验收

`jsonEquals` 将解析后的 JSON 值与人工固定的交付合同比较，不再靠正文里是否出现数字判断。
验收条件示例：

```json
{"kind":"jsonEquals","path":"report.json","pointer":"/totals/approved","expectedJson":"42"}
```

`pointer` 遵循 RFC6901（`~1` 表示斜杠，`~0` 表示波浪号）；空指针选择整个文档。
`expectedJson` 是编码后的 JSON 值，最多 16,384 个 UTF-8 字节，因此 `"null"` 与未声明期望值
可以区分。缺失字段不等于 null，字符串不等于数字。按解析后 JSON 值比较，不提供财务十进制运算。
文档必须是任务工作区内、不超过 16 MiB 的常规 UTF-8 JSON 文件。无效指针、期望值、文档、
路径或不可用的测量均不通过验收。

比较与 SHA-256 测量使用同一份捕获字节。既有验证回执把该版本绑定到任务、run 与合同；
多个检查观察到互相矛盾的文件版本时，不能提交成功证据。它证明文档符合给定值，
不证明外部来源真实或远端事务已成功。
## 普通聊天操作核对

使用会话顶栏的「核对聊天操作」，或人工专用 `turn/operations` / `turn/reconcile`。
查询限定在 `sessionId`，可选 `turnId` 指定已停止的普通聊天检查点。属于任务的操作继续使用
`task/operations` / `task/reconcile`。跨多个回合查询时，每个操作携带自己的检查点摘要；
客户端必须提交选中操作对应的摘要。

同一会话的新循环不能绕过此前未知的外部结果：仅保留已知读取工具，并带有限的宿主台账说明。
覆盖没有操作元数据的旧检查点，以及启动期间尚未被领取的 open 边界。人工查询会先停止无人
执行且结果不明的边界，再重建有来源标记的元数据；这不证明请求已经发出。旧消息损坏时仍拒绝修改。

证据不可变、限定确切身份，并与检查点原子提交。同一结果和证据可重复确认；过期摘要或冲突结果
被拒绝。旧回合保持停止。核对后请在当前工作目录下发送新指令；此入口不会恢复旧工作目录或权限。
人工观察不计为实际工具成功。不同会话和非协作外部进程仍没有全局外部副作用去重保证。
