# 协议参考（app-server v2）

[English](protocol.md) | 简体中文

`joy app-server` 在 stdin/stdout 上说**换行分隔的 JSON-RPC 2.0**：一行一个
请求，应答与通知混序写出（通知先于应答）。所有类型定义在
`joyczl-protocol/src/protocol/v2.rs`（63 个类型），并生成 TypeScript
（`schema/typescript/v2/`）与 Python pydantic（`sdk/python/generated/`）。
**改协议必须跑 `just write-app-server-schema`。**

## 方法一览（13 个）

| 方法 | 请求 → 应答 | 说明 |
|---|---|---|
| `turn/start` | `TurnStartParams` → `TurnStartResponse` | 跑一轮对话；应答在全部通知之后发出 |
| `turn/interrupt` | `TurnInterruptParams` → `TurnInterruptResponse` |
| `goal/set`（设/清一个目标）、通知 `GoalRound`（每判一次发一条）：

```
you>  /goal 让测试全绿
       → goal/set  {sessionId, condition: "让测试全绿"}
       ← {active: true, condition: "让测试全绿", roundsUsed: 0, maxRounds: 5}
… 一轮跑完，判断器说不算达成 …
       ← GoalRound {turnId, round: 1, maxRounds: 5, status: "continuing", reason: "还有两条红"}
… 又跑一轮 …
       ← GoalRound {turnId, round: 2, maxRounds: 5, status: "satisfied", reason: "全绿了"}
```

`TurnMeta.goalStatus` / `goalRounds` 把出口与轮次也带进这一轮的元数据。

`approval/respond` | `ApprovalRespondParams` → `ApprovalRespondResponse` | 回答一次「要不要执行」；太晚送达时 `accepted: false` | 打断在跑的 turn；`interrupted:false` = 已跑完 |
| `session/new` | `SessionNewParams` → `SessionNewResponse` | 新会话标签（不建任何实体） |
| `session/list` | `SessionListParams` → `SessionListResponse` | 会话列表（游标分页） |
| `session/messages` | `SessionMessagesParams` → `SessionMessagesResponse` | 一个会话的消息，最新在前，往上翻页 |
| `memory/search` | `MemorySearchParams` → `MemorySearchResponse` | facts + episodes 关键词检索 |
| `memory/list` | `MemoryListParams` → `MemoryListResponse` | 最近事实（游标分页） |
| `memory/list-episodes` | `MemoryListEpisodesParams` → `MemoryListEpisodesResponse` | 最近情景 |
| `memory/remember` | `MemoryRememberParams` → `MemoryRememberResponse` | 写一条事实 |
| `memory/forget` | `MemoryForgetParams` → `MemoryForgetResponse` | 按主题删除 |
| `config/read` | `ConfigReadParams` → `ConfigReadResponse` | 完整配置视图 |
| `config/write` | `ConfigWriteParams` → `ConfigWriteResponse` | 应用补丁并热生效（落 settings.json） |
| `model/list` | `ModelListParams` → `ModelListResponse` | 12 家 provider 的模型目录 |

约定：方法名 `<resource>/<method>`（resource 单数）；载荷命名
`*Params` / `*Response` / `*Notification`；wire 字段 camelCase；整数一律
`i32`（`i64` 会被 ts-rs 映射成 `bigint`，与 `JSON.parse` 的 `number`
运行时不符）；列表方法一律 `cursor`/`limit` → `data` + `next_cursor`。

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
| `-32002` NOT_IMPLEMENTED | 协议定义了但尚未实现（当前 13 个方法已全部实现） |

## 客户端

* TypeScript：`joy-ts/packages/client`（`StdioTransport` + `JoyClient`，
  通知按判别式 `type` 分发）。
* Python：`sdk/python/joyczl_agent`（`JoyClient.connect()` 拉起子进程，
  pydantic 运行时校验；通知判别用 `n.type`，不要用 isinstance）。
* 终端 REPL 与 dashboard 走进程内/HTTP 翻译的同一套方法面。
