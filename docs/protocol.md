# Protocol reference (app-server v2)

English | [简体中文](protocol.zh.md)

`joy app-server` speaks **newline-delimited JSON-RPC 2.0** on stdin/stdout:
one request per line; responses and notifications interleave (notifications
before the response). Every type is defined in
`joyczl-protocol/src/protocol/v2.rs` (130 types, JSON-RPC envelopes included) and generated into TypeScript
(`schema/typescript/v2/`).
**Changing the protocol requires `just write-app-server-schema`.**

## Methods (41)

| Method | Request → Response | Purpose |
|---|---|---|
| `turn/operations` | `TurnOperationsParams` → `TaskOperationsResponse` | inspect ordinary-chat uncertainty in one session, optionally one turn |
| `turn/reconcile` | `TurnReconcileParams` → `TaskOperationsResponse` | human evidence for an exact ordinary-chat operation; no execution or resume |
| `turn/start` | `TurnStartParams` → `TurnStartResponse` | run one conversation turn; the response follows all notifications |
| `turn/interrupt` | `TurnInterruptParams` → `TurnInterruptResponse` | cancel a running turn; `interrupted:false` = already finished |
| `turn/steer` | `TurnSteerParams` → `TurnSteerResponse` | add a line to the **running** turn: it lands at the next round boundary, finished tools are never re-run |
| `turn/follow-up` | `TurnFollowUpParams` → `TurnFollowUpResponse` | have the server start the next turn after this one (per-session subscription) |
| `turn/list` | `TurnListParams` → `TurnListResponse` | turns currently running, filterable by `sessionId` |
| `approval/respond` | `ApprovalRespondParams` → `ApprovalRespondResponse` | answer one approval request; `accepted: false` when it arrived too late |
| `approval/log` | `ApprovalLogParams` → `ApprovalLogResponse` | the approval audit (migration 0011): the last 100 records, four outcomes `approved` / `denied` / `timeout` / `voided` |
| `todo/view` | `SessionViewParams` → `TodoListView` | the current todo sheet; nothing renders when the model never wrote one |
| `task/create` | `TaskCreateParams` → `TaskEnvelope` | create a task: session optional, with acceptance, budget and working directory |
| `task/start` | `TaskStartParams` → `TaskEnvelope` | human-authorized durable start in a separate execution session |
| `task/wait` | `TaskWaitParams` → `TaskEnvelope` | declare one immutable external input condition before starting |
| `task/signal` | `TaskSignalParams` → `TaskSignalResponse` | supply one correlated input event through an authorized host |
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
| `memory/setValidity` | `MemoryValidityParams` → `MemoryValidityResponse` | human replaces fact validity bounds |
| `memory/provenance` | `MemoryProvenanceParams` → `MemoryProvenanceResponse` | inspect validity and extraction contexts |
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
| `memory/ledger` | `MemoryLedgerParams` → `MemoryLedgerResponse` | one session's ledger, newest first; with `turnId` also that turn's entries. Read-only |
| `memory/index` | — → `MemoryIndexResponse` | the bundle's catalog (`index.md`) plus one summary per page. Read-only |
| `memory/page` | `MemoryPageParams` → `MemoryPageResponse` | one page with body and `sources`; `page: null` when there is no such page |
| `context/usage` | `ContextUsageParams` → `ContextUsageResponse` | the last turn's six-section breakdown; `usage: null` = nothing measured yet |
| `skill/stats` | — → `SkillStatsResponse` | aggregated `<home>/skill-hits.jsonl`. Read-only |
| `session/compact` | `SessionCompactParams` → `SessionCompactResponse` | fold older turns into the rolling summary **now**, keeping the newest two. Human-only — no tool reaches it |

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
* `cwd` (optional): the turn's working directory. Must be an absolute path that exists (fail-closed); `run_command` resolves its fallback chain as `args.cwd > cwd > process cwd`, and relative paths in `read_file`/`list_dir`/`glob`/`grep`/`delete_file`/`move_file` resolve against it (session-level working-directory). Subagents inherit the parent turn's cwd.
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
| `-32002` NOT_IMPLEMENTED | defined but not yet implemented (all 45 methods are implemented) |

## Clients

* TypeScript: `joy-ts/packages/client` (`StdioTransport` + `JoyClient`;
  notifications dispatch on the `type` discriminant).
* The terminal REPL and dashboard drive the same method surface in-process
  and over HTTP respectively.

## Independent task execution

`task/create` only records a task. A human can call `task/start {taskId, message}` (or click **Start task** in the cockpit) to authorize its initial execution. The message contains the actual instructions; the cockpit uses the title and goal. One SQLite-backed worker consumes these requests. Creation alone never starts work, and there is no model tool for starting tasks.

`AgentTaskView.executionSessionId` identifies its separate transcript; `sessionId` remains the originating conversation. `dispatchState` is pending / claimed / finished / interrupted, describing initial dispatch, not the task's final outcome. Paused and job-waiting tasks keep their normal task states. The original conversation can continue independently and receives bounded, owner-scoped task progress context. Working transcripts are not copied into chat. Output without acceptance is described as unverified in that progress snapshot.

Each task accepts one initial dispatch; a duplicate start is rejected. Pending dispatches survive restart. Claimed work is owned by checkpoint recovery; a claim with no checkpoint stops for review rather than being submitted again. A queued pause requires human resume; cancel prevents execution. `POST /api/task` accepts `action: "start"` with the same parameters. The TypeScript client exposes `TASK_START`.

This is a local worker, not an always-online service or a distributed lease system. It runs while app-server is alive and a model is configured. It does not automatically ingest external callbacks or grant capabilities. Unrelated manual turns retain their existing concurrency rules; concurrent changes to the same external resource still require coordination.

## Durable external input

Before starting a task, a human or authorized host may declare `task/wait {taskId, source, eventKey, description, expiresAt?}`. The condition is immutable and limited to one per task. `expiresAt` is an RFC 3339 input-arrival deadline, not a deadline for processing an already accepted event. An incomplete task can save its draft and suspend without consuming more model rounds while waiting. Companion chat remains independent.

`task/signal {conditionId, source, eventKey, eventId, data}` supplies a JSON value up to 16 KiB. Source and event key must match. Repeating the same source/event ID and data acknowledges the original event; changing its target or data is rejected. These fields correlate data; they do not authenticate a sender. Use the existing authenticated host transport, never expose this method as a public webhook or a model tool.

A committed event wakes the exact waiting run once, including after restart. Its checkpoint retains the draft and full conversation; the input is explicitly untrusted data and grants no permissions. Input receipt is not task completion: the resumed execution must pass its original budget and acceptance checks. Human pause/cancel wins. Input received during a human pause remains stored until explicit `task/resume`; continuation waits for the previous actor to finish before committing its authorization and reopened checkpoint atomically.

## Operation journal and human reconciliation

`task/operations {taskId}` returns persisted operation identities, argument/output digests, intent or observed result, human evidence and the current checkpoint digest. Local operation keys are not provider idempotency keys. The automatic replay policy is `never`; an intent means execution may or may not have happened.

A human may stop the task, inspect the real external state, then call `task/reconcile {taskId, operationId, checkpointDigest, resolution, evidence}`. Resolution is `reconciled_success`, `reconciled_failure` or `not_executed`; evidence is 1–16384 bytes. The latest suspended execution and exact unresolved call/arguments must match. The evidence and a labelled tool-result receipt commit atomically with the checkpoint. Identical evidence can be acknowledged again; conflicting settlements are refused.

Reconciliation never executes a tool, clears a human pause or resumes a task. Every unresolved call must be reconciled before explicit `task/resume`. Human confirmation is not an observed successful tool return and does not satisfy `toolsOk`; original artifact and budget checks still apply. Recent operation evidence is reconstructed into durable handoffs independently of rolling summaries. These methods are authenticated human/host controls, never model tools.

`ToolOperationView.origin` distinguishes an intent committed with its execution checkpoint (`checkpoint`) from pending metadata reconstructed from an unchanged stopped legacy checkpoint (`recovered_checkpoint`). Neither proves transmission. Human pause atomically fences the task, cancels durable waits and suspends its recoverable boundary; a failed commit preserves the original wait.

## Versioned verification evidence

`CheckView.artifactSha256`, when present, identifies the exact bytes read for a file measurement. File existence checks stream a regular file; substring checks use the same stable text read for matching and hashing. A changing or unreadable file fails its check. Historical fingerprints do not assert that a path still contains those bytes.

Nonempty acceptance results now commit with an immutable task/run/round verification receipt (migration 0038), its file manifest and the current checks projection. A failed evidence commit stops completion for human review. Changed content creates another linked receipt, including an A→B→A reversion; immediate identical observations reuse the latest receipt. Receipt integrity covers its measurements, observation time and predecessor ID. A successful task completion receipt binds the corresponding exact-run verification ID. No archival file copy is created.

A model-only goal judgement stops the goal loop but does not establish independent acceptance. With no acceptance conditions, completion receipts and conversation progress use `produced_unverified`, and the cockpit labels the result accordingly even when a goal condition exists.

## Memory validity and derivation context

Human clients can replace a fact's whole validity window with `memory/setValidity`
(`MemoryValidityParams` → `MemoryValidityResponse`). Supply `factId`, `validFrom` and
`validUntil`; timestamps use RFC3339, null removes a bound, and an end must follow its
start. The start is inclusive and the end exclusive. These controls are not model tools.
`memory/provenance` (`MemoryProvenanceParams` → `MemoryProvenanceResponse`) reads the
bounds and recorded extraction contexts, including inactive facts when their ID is known.

Migration 0039 records the claimed chat row IDs and SHA-256 digests of their role/content
as derivation context. This identifies the model's input batch, not which sentence proves
a fact. Replaying that exact context does not increase its confirmation count. Corrections
clear the previous claim's contexts; deletion cascades them. Legacy facts may have none.

Migration 0040 filters inactive facts from keyword/vector/recent recall. Bounded facts
stay out of generated static mirrors even while active. Prompt catalogs and
`read_memory_page` check database-backed page sources at read time; missing, corrected or
bounded sources exclude the whole snapshot. Human pages without fact references remain
independent. Existing files, chat history, episodes, checkpoints and backups are not erased
or retroactively redacted. The temporal filter covers facts, not episodic summaries.

Migration 0041 adds source-bound temporary observations to background consolidation.
The extraction contract separates durable `facts` from optional `states`. Each state carries
an exact `sourceRef`, a 4–400 character user quotation, model-reported confidence in [0,1]
and a lifetime of 1–24 hours. The reference must belong to the claimed input batch and match
the current stored user row's SHA-256; assistant text and mismatched quotations are refused.
Expiry is anchored to the source message, not extraction/retry time. Already expired sources
produce no temporary fact. Fact, validity and assessment commit atomically.

`memory/provenance` additionally returns optional `validityOrigin` (`human` or `consolidation`)
and each observation's optional `assessmentJson`, including quotation, source reference,
confidence and source-time expiry. Confidence is uncalibrated model judgment; matching a quote
does not verify the inferred meaning or a person's current mood. Replaying an observation does
not renew it. Human content/validity edits and deletions take priority; legacy unbounded claims
are not automatically reclassified. These metadata fields add no model control method.

Recall labels bounded claims as temporary and episodes as historical summaries. Fact validity
does not redact raw history or expire episodes. Model misclassification, semantic paraphrases,
explicit note tools and historical unbounded moods remain outside this automatic TTL guarantee.
Malformed extraction shapes retain source chats with retry backoff instead of silently marking
them consumed. Extraction instructions and JSON exchange data use separate message roles.

Migration 0042 distinguishes admitted extraction batches from completed rows. Exclusive
runtime startup releases only recorded unfinished claims. A retry retains its original source
set, even when new chats arrive or the new-work threshold changes; fresh admissions contain
at most 4,096 rows. Backoff updates the whole batch transactionally. A committed source-context
receipt prevents a repeated batch from inserting or replacing its episode summary, and survives
human episode deletion as a tombstone. Completed rows and legacy rows without admission metadata
are not reopened. The upgrade cannot identify pre-migration abandoned claims.

The memory worker retains database execution ownership while alive. Graceful shutdown waits
briefly, then cancels and joins a stalled extraction; dropping the worker aborts its loop.
A recovered model extraction may issue another read-only model request and incur usage; it
never replays task tools. Unreceived provider usage remains outside exact task accounting.

## Structured document acceptance

`jsonEquals` checks a parsed JSON value against the human's fixed contract, rather than
searching text for a number. Example acceptance criterion:

```json
{"kind":"jsonEquals","path":"report.json","pointer":"/totals/approved","expectedJson":"42"}
```

`pointer` uses RFC6901 (`~1` for slash, `~0` for tilde); an empty pointer selects the whole
document. `expectedJson` is an encoded JSON value, limited to 16,384 UTF-8 bytes, so `"null"`
is distinguishable from an omitted expectation. Missing fields do not equal null, and strings
do not equal numbers. Equality follows parsed JSON values, not financial decimal arithmetic.
Documents must be regular UTF-8 JSON files within the task workspace and at most 16 MiB.
Invalid pointers, expectations, documents, paths and unavailable measurements fail closed.

The check and SHA-256 measurement use the same captured bytes. Existing verification receipts
bind that version to the task/run/contract; contradictory versions across checks cannot be
committed as successful evidence. This proves conformance to supplied values, not the truth
of an external source or a successful remote transaction.

## Ordinary-chat operation review

Use **Review chat operations** in the conversation header, or the human-only
`turn/operations` / `turn/reconcile` methods. Inspection is scoped to `sessionId`;
optional `turnId` selects a stopped ordinary-chat checkpoint. Task-owned operations continue
to use `task/operations` and `task/reconcile`. Each operation carries its own checkpoint digest
when an inspection spans multiple turns; clients must submit the selected operation's digest.

A new loop in the same conversation cannot bypass unresolved prior effects: it keeps only
known read tools and carries a bounded host bookkeeping note. This includes legacy checkpoints
without journal metadata and unclaimed open boundaries during startup. Human inspection stops
an unclaimed uncertain boundary before reconstructing labelled metadata; it never proves that
a request was transmitted. Corrupt old messages keep mutation access closed.

Evidence is immutable, identity-scoped and committed atomically with the exact checkpoint.
Same evidence/outcome can be acknowledged again; a stale digest or conflicting outcome is
rejected. The old turn stays stopped. After review, send a new instruction using the current
workspace; this route does not recover old permissions or workspace state. Human observations
do not count as observed tool successes. Separate conversations and uncooperative external
processes still have no global external-effect deduplication guarantee.

### Explicit continuation after a stop

`turn/interrupt` stops execution and does not automatically resume its checkpoint. A human may explicitly call `task/resume` for a known-outcome stopped checkpoint belonging to the latest task execution; authorization, activation and reopening commit atomically. Cancelled tasks, stale execution identities and unreconciled unknown tool outcomes remain blocked.
