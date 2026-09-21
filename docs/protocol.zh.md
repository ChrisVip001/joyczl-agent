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
| `turn/interrupt` | `TurnInterruptParams` → `TurnInterruptResponse` | 打断在跑的 turn；`interrupted:false` = 已跑完 |
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
turnStarted → gateDecided → textDelta* → toolStarted → toolCompleted*
            → consolidationCompleted? → turnCompleted →（应答）
```

* `gateDecided`：检索门判定（retrieve/skip + 理由 + 检索词）。
* `textDelta`：流式文本增量；仅 `stream:true` 时产生。
* `toolStarted` / `toolCompleted`：工具开始与完成（含耗时与状态）。
* `turnCompleted`：`reply`、`iterations`、`usage`、`meta`（gate / graph /
  tools / model / provider / latency / interrupted）。meta 随对话落库。

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
