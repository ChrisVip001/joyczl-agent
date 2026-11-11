# Protocol reference (app-server v2)

English | [简体中文](protocol.zh.md)

`joy app-server` speaks **newline-delimited JSON-RPC 2.0** on stdin/stdout:
one request per line; responses and notifications interleave (notifications
before the response). Every type is defined in
`joyczl-protocol/src/protocol/v2.rs` (130 types, JSON-RPC envelopes included) and generated into TypeScript
(`schema/typescript/v2/`) and Python pydantic (`sdk/python/generated/`).
**Changing the protocol requires `just write-app-server-schema`.**

## Methods (41)

| Method | Request → Response | Purpose |
|---|---|---|
| `turn/start` | `TurnStartParams` → `TurnStartResponse` | run one conversation turn; the response follows all notifications |
| `turn/interrupt` | `TurnInterruptParams` → `TurnInterruptResponse` | cancel a running turn; `interrupted:false` = already finished |
| `turn/steer` | `TurnSteerParams` → `TurnSteerResponse` | add a line to the **running** turn: it lands at the next round boundary, finished tools are never re-run |
| `turn/follow-up` | `TurnFollowUpParams` → `TurnFollowUpResponse` | have the server start the next turn after this one (per-session subscription) |
| `turn/list` | `TurnListParams` → `TurnListResponse` | turns currently running, filterable by `sessionId` |
| `approval/respond` | `ApprovalRespondParams` → `ApprovalRespondResponse` | answer one approval request; `accepted: false` when it arrived too late |
| `approval/log` | `ApprovalLogParams` → `ApprovalLogResponse` | the approval audit (migration 0011): the last 100 records, four outcomes `approved` / `denied` / `timeout` / `voided` |
| `todo/view` | `SessionViewParams` → `TodoListView` | the current todo sheet; nothing renders when the model never wrote one |
| `task/create` | `TaskCreateParams` → `TaskEnvelope` | create a task: session optional, with acceptance, budget and working directory |
| `task/list` | — → `TaskListResponse` | every task, newest first |
| `task/view` | `SessionViewParams` → `TaskViewResponse` | the session's current task card |
| `task/get` | `TaskGetParams` → `TaskEnvelope` | one task: acceptance, budget, spend, run count |
| `task/runs` | `TaskGetParams` → `TaskRunsResponse` | execution records: when each run started, ended, and why it stopped |
| `task/artifacts` | `TaskGetParams` → `TaskArtifactsResponse` | the deliverables (files that really exist) |
| `task/suspend` | `TaskActionParams` → `TaskEnvelope` | pause: keep the checkpoint, lose no finished tool |
| `task/resume` | `TaskActionParams` → `TaskEnvelope` | continue from the checkpoint |
| `task/cancel` | `TaskActionParams` → `TaskEnvelope` | cancel (terminal, no auto-continue) |
| `inbox/list` | — → `InboxListView` | connector inbox: the newest mail plus how many still wait for a person |
| `inbox/summarize` | `InboxSummarizeParams` → `InboxSummarizeResponse` | turn the inbox into one task round |
| `session/new` | `SessionNewParams` → `SessionNewResponse` | create a session: a real row, optionally with a name and working directory |
| `session/list` | `SessionListParams` → `SessionListResponse` | session list (cursor-paged) |
| `session/get` | `SessionGetParams` → `SessionGetResponse` | one session's metadata |
| `session/update` | `SessionUpdateParams` → `SessionUpdateResponse` | rename / change directory (`null` = leave alone, `""` = clear) |
| `session/messages` | `SessionMessagesParams` → `SessionMessagesResponse` | a conversation's messages, newest first, paging upward |
| `memory/search` | `MemorySearchParams` → `MemorySearchResponse` | keyword search over facts + episodes |
| `memory/list` | `MemoryListParams` → `MemoryListResponse` | recent facts (cursor-paged) |
| `memory/list-episodes` | `MemoryListEpisodesParams` → `MemoryListEpisodesResponse` | recent episodes |
| `memory/remember` | `MemoryRememberParams` → `MemoryRememberResponse` | store a fact |
| `memory/forget` | `MemoryForgetParams` → `MemoryForgetResponse` | delete by subject |
| `config/read` | `ConfigReadParams` → `ConfigReadResponse` | full configuration view |
| `config/write` | `ConfigWriteParams` → `ConfigWriteResponse` | apply a patch, hot-reload (persisted to settings.json) |
| `model/list` | `ModelListParams` → `ModelListResponse` | model catalog across 12 providers |
| `model/profiles` | `ModelProfilesParams` → `ModelProfilesResponse` | configured profiles + the active one (**no secrets**, only a "where the key lives" pointer) |
| `model/save` | `ModelSaveParams` → `ModelSaveResponse` | create/update a profile and make it active; `id` + `makeActive` alone = switch |
| `model/remove` | `ModelRemoveParams` → `ModelRemoveResponse` | delete a profile and its credential |
| `model/routes` | — → `ModelRoutesResponse` | the routing table: `fixed` / `auto` plus three "tier → profile id" rows (an unconfigured tier is `null`) |
| `model/routes/save` | `ModelRoutesSaveParams` → `ModelRoutesResponse` | replace the whole table; tier names and profile ids are validated before writing |
| `usage/summary` | `UsageSummaryParams` → `UsageSummaryResponse` | the usage ledger (migration 0010): per-model aggregates + totals. **Accounting only, no enforcement** |
| `schedule/list` | `ScheduleListParams` → `ScheduleListResponse` | the schedule snapshot (migration 0012): per-job `last_fired` / `claimed` / `catch_up` / `delivery`. Read-only |

Conventions: methods are `<resource>/<method>` (singular resource); payloads
are named `*Params` / `*Response` / `*Notification`; wire fields are
camelCase; integers are `i32` only (`i64` maps to ts-rs `bigint` while
`JSON.parse` yields `number` — a runtime mismatch); list methods always use
`cursor`/`limit` → `data` + `next_cursor`.

### Using the goal loop

A goal is **not an object of its own**: it is a column on the task row (`goalCondition`, since
0.17), which is why there is no `goal/set` / `goal/view` method — a human writes it through
`/goal` or `task/create {goalCondition}`, and the loop reads the same row. The only protocol
surface left is the `GoalRound` notification (one per judgement):

```
you>  /goal make the tests green
       → task/create  {sessionId: "…", title: "make the tests green", goalCondition: "make the tests green"}
… the turn ends without satisfying it …
       ← GoalRound {turnId, round: 1, maxRounds: 5, status: "continuing", reason: "two red"}
… another round …
       ← GoalRound {turnId, round: 2, maxRounds: 5, status: "satisfied", reason: "all green"}
```

`TurnMeta.goalStatus` / `goalRounds` carry the outcome and the count into the turn's
metadata. `TurnMeta.routeReason` (optional) says how the model was chosen this turn —
`auto:reasoning → ds-r1` or `fallback: …`; empty when nothing was chosen.

## turn/start notification sequence

```
turnStarted → gateDecided → retry* → textDelta* → toolStarted → toolCompleted*
            → consolidationCompleted? → turnCompleted → (response)
```

`retry` can precede any model call (gate, loop, summarizer), so it is
**insertable** rather than fixed in place.

* `gateDecided`: the retrieval gate's ruling (retrieve/skip + reason + query).
* `retry`: one retry (attempt, reason, delay). Emitted **every time** — a retry
  is never silent; `meta.retries` carries the total.
* `approvalRequested`: a command is waiting for approval (tool, the command
  itself, why, deadline). Answer it with `approval/respond` or the turn stays
  blocked; **no answer is a refusal**.
* `textDelta`: streamed text increment; only with `stream:true`.
* `cwd` (optional): the turn's working directory. Must be an absolute path that exists (fail-closed); `run_command` resolves its fallback chain as `args.cwd > cwd > process cwd`, and relative paths in `read_file`/`list_dir` resolve against it (session-level working-directory). Subagents inherit the parent turn's cwd.
* `toolStarted` / `toolCompleted`: tool begin and end (duration, status).
* `turnCompleted`: `reply`, `iterations`, `usage`, `meta` (gate / graph /
  tools / model / provider / latency / interrupted / guardHits / retries).
  The meta is persisted with the conversation.

### `Fact.kind`

What a memory is about: `user` / `feedback` / `project` / `reference` /
`fact` (the fallback). It travels in `memory/search` and `memory/list`
responses and in the retrieval context; `memory/remember` may send it, and an
unknown value is normalised to `fact`.

## Error codes

| Code | Meaning |
|---|---|
| -32700 / -32600 / -32601 / -32602 / -32603 | standard JSON-RPC: parse / request / method not found / params / internal |
| `-32000` PROVIDER_ERROR | model-side failure (missing key, 401, rate limit…), with what to do |
| `-32001` TOOL_ERROR | tool execution failed (the error also returns to the model as text; the turn continues) |
| `-32002` NOT_IMPLEMENTED | defined but not yet implemented (all 39 methods are implemented) |

## Clients

* TypeScript: `joy-ts/packages/client` (`StdioTransport` + `JoyClient`;
  notifications dispatch on the `type` discriminant).
* Python: `sdk/python/joyczl_agent` (`JoyClient.connect()` spawns the
  subprocess, pydantic validates at runtime; discriminate notifications with
  `n.type`, never isinstance).
* The terminal REPL and dashboard drive the same method surface in-process
  and over HTTP respectively.
