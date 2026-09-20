# Protocol reference (app-server v2)

English | [简体中文](protocol.zh.md)

`joy app-server` speaks **newline-delimited JSON-RPC 2.0** on stdin/stdout:
one request per line; responses and notifications interleave (notifications
before the response). Every type is defined in
`joyczl-protocol/src/protocol/v2.rs` (63 types) and generated into TypeScript
(`schema/typescript/v2/`) and Python pydantic (`sdk/python/generated/`).
**Changing the protocol requires `just write-app-server-schema`.**

## Methods (13)

| Method | Request → Response | Purpose |
|---|---|---|
| `turn/start` | `TurnStartParams` → `TurnStartResponse` | run one conversation turn; the response follows all notifications |
| `turn/interrupt` | `TurnInterruptParams` → `TurnInterruptResponse` | cancel a running turn; `interrupted:false` = already finished |
| `session/new` | `SessionNewParams` → `SessionNewResponse` | new session label (creates nothing) |
| `session/list` | `SessionListParams` → `SessionListResponse` | session list (cursor-paged) |
| `session/messages` | `SessionMessagesParams` → `SessionMessagesResponse` | a conversation's messages, newest first, paging upward |
| `memory/search` | `MemorySearchParams` → `MemorySearchResponse` | keyword search over facts + episodes |
| `memory/list` | `MemoryListParams` → `MemoryListResponse` | recent facts (cursor-paged) |
| `memory/list-episodes` | `MemoryListEpisodesParams` → `MemoryListEpisodesResponse` | recent episodes |
| `memory/remember` | `MemoryRememberParams` → `MemoryRememberResponse` | store a fact |
| `memory/forget` | `MemoryForgetParams` → `MemoryForgetResponse` | delete by subject |
| `config/read` | `ConfigReadParams` → `ConfigReadResponse` | full configuration view |
| `config/write` | `ConfigWriteParams` → `ConfigWriteResponse` | apply a patch, hot-reload (persisted to settings.json) |
| `model/list` | `ModelListParams` → `ModelListResponse` | model catalog across 11 providers |

Conventions: methods are `<resource>/<method>` (singular resource); payloads
are named `*Params` / `*Response` / `*Notification`; wire fields are
camelCase; integers are `i32` only (`i64` maps to ts-rs `bigint` while
`JSON.parse` yields `number` — a runtime mismatch); list methods always use
`cursor`/`limit` → `data` + `next_cursor`.

## turn/start notification sequence

```
turnStarted → gateDecided → textDelta* → toolStarted → toolCompleted*
            → consolidationCompleted? → turnCompleted → (response)
```

* `gateDecided`: the retrieval gate's ruling (retrieve/skip + reason + query).
* `textDelta`: streamed text increment; only with `stream:true`.
* `toolStarted` / `toolCompleted`: tool begin and end (duration, status).
* `turnCompleted`: `reply`, `iterations`, `usage`, `meta` (gate / graph /
  tools / model / provider / latency / interrupted). The meta is persisted
  with the conversation.

## Error codes

| Code | Meaning |
|---|---|
| -32700 / -32600 / -32601 / -32602 / -32603 | standard JSON-RPC: parse / request / method not found / params / internal |
| `-32000` PROVIDER_ERROR | model-side failure (missing key, 401, rate limit…), with what to do |
| `-32001` TOOL_ERROR | tool execution failed (the error also returns to the model as text; the turn continues) |
| `-32002` NOT_IMPLEMENTED | defined but not yet implemented (all 13 methods are implemented) |

## Clients

* TypeScript: `joy-ts/packages/client` (`StdioTransport` + `JoyClient`;
  notifications dispatch on the `type` discriminant).
* Python: `sdk/python/joyczl_agent` (`JoyClient.connect()` spawns the
  subprocess, pydantic validates at runtime; discriminate notifications with
  `n.type`, never isinstance).
* The terminal REPL and dashboard drive the same method surface in-process
  and over HTTP respectively.
