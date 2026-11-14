# Harness upgrade implementation

English | [简体中文](harness-upgrade.zh.md)

Working implementation checklist for the 2026-11-12 architecture assessment. This is a continuation record, not a claim that the full assessment has been implemented. Preserve the Rust runtime, SQLite state, human-controlled authorization and generated protocol.

## Implemented and covered by regression tests

- Checkpoint execution intent before tool batches; failed checkpoint persistence prevents execution. Unknown in-flight batches are suspended rather than automatically replayed.
- Admit the explicit round number atomically with the execution checkpoint (migration 0031); recovery uses that number instead of a potentially stale task counter. Round-ledger failure stops execution.
- Keep loop-round identity through checkpoints and continuation. Atomically charge task/run totals from cumulative round watermarks; accounting failure stops the task. Legacy checkpoints with already-accounted runs require review.
- Recover a task through its checkpoint's run identity. Fresh and recovered tasks share one accounting, budget, acceptance and goal driver; recovered goals can continue within the authorized round cap.
- Allocate background-job identities independently of process counters. Persist intent before launching a process; failure to write the ledger prevents launch.
- Drain process output before publishing job completion, with a bounded wait and explicit incomplete-output marker. Bind jobs to exact host task/run identities. Commit job outcomes, task timeline events and requested-delivery receipts together.
- Persist exact job dependency sets and resume authorized tasks after known completion, including restart. Human pause/cancel wins; interrupted or killed jobs require review. Competing wake workers claim once.
- Claim follow-up instructions atomically and preserve them until the outcome is recorded. Restarted unknown claims are retained for attention and reported in chat; concurrent drains cannot take the same instruction.
- Preserve human memory deletion/correction against matching automatic re-extraction, including persistent subject-wide exclusion. Corrected facts discard stale embeddings; explicit memory writes remain possible.
- Discover Chinese skills through distinct adjacent Han-character pairs while retaining ASCII matching and explicit-only restrictions.
- Include session-local recent messages and SOUL in quick replies; keep other sessions out.
- Add human-only durable `task/start`, one local worker, independent task transcripts, queued pause/resume/cancel and owner-scoped progress projections. Pending starts survive restart; checkpoint recovery owns claimed work. See [execution contract](protocol.md#independent-task-execution).
- Reconnect failed MCP transports without replaying unknown calls. A structured uncertainty signal stops remaining tools and model continuation; the pending intent remains suspended for reconciliation.
- Preserve operation numbering across goal rounds so later operations are not silently deduplicated.
- Commit whole immutable plan versions with their readable steps; restore them for fresh/recovered tasks and subsequent goal rounds.
- Rebuild schema-1 task handoffs from persistent contract, budget, plan, step and artifact records every round. Historical evidence is labelled; corrupt, foreign or oversized handoffs stop execution rather than losing constraints.
- Resolve handoffs by explicit turn/run bindings, rejecting ambiguous identities. Strictly validate stored contracts before planning/model calls; charge known recovered token usage before budget admission.
- Scope successful-tool acceptance to the current run. Truncated or iteration-limited responses cannot finish tasks.
- Preserve native Responses encrypted continuation and DeepSeek thinking continuation.
- Run a budgeted real-model smoke using profile `ds` / `deepseek-flash`: three confirmed scenarios, with raw evidence and costs in [live reliability](live-reliability.md).
- Preserve usable recovery paths in short tool-output previews.
- Claim scheduled triggers atomically, quarantine unknown results and persist immutable output/settlement/delivery receipts together. Redelivery never reruns the model; conflicting human files are retained.
- Close exact terminal runs and produce task/goal completion receipts atomically with their committed task status; failed commits cannot advertise completion. Preserve the original delivery target and retry enqueue failures after restart.
- Persist configured completion notices before sending, retain failures for periodic/restart retry, and delete only after acknowledgement. Approval prompts are not replayed. Transport errors do not expose webhook credentials.

## Remaining work, in order

1. Extend round accounting fault tests to crash boundaries between observation, accounting and finalization. Active wall time before an uncheckpointed crash remains approximate; it is not a hard real-time cap.
2. Structured handoffs now reconstruct the durable contract, plan, steps and artifact references independently of summaries. Extend domain evidence versions and reconciled external receipts; ordinary chat auto-continuation still has separate pre-driver handling.
3. Durable waits now cover background jobs and one human-declared external input, including restart and persistent human pause priority. Broaden domain-specific event adapters only after their authentication and reconciliation contracts are defined. Schedule outcomes and task/goal transitions already atomically produce settlement/delivery receipts.
4. Operation state and human reconciliation now cover current task runs. Ordinary-chat reconciliation and mutation fences are also implemented. Add provider-supported idempotency keys only with explicit replay contracts; arbitrary external effects still have no exactly-once guarantee.
5. File acceptance now has versioned, digest-checked evidence bound to completion receipts, and unverified production is labelled. Extend domain-specific checks and remote read-back receipts; file versions alone do not establish domain truth.
6. Independent tasks, owner-scoped progress, common database execution ownership and cooperating runtime mutation coordination are implemented. Extend richer relationship continuity and conflict-aware resource scopes; quick replies now retain bounded recent context and saved personal preferences; broader relationship quality remains unmeasured.
7. Memory suppression/correction lineage (0032), derivation contexts (0039) and human validity windows (0040) are implemented. Source-bound transient classification and expiry (0041) are implemented; extend classification-quality validation and review of historical metadata. Semantic paraphrases, pre-migration deletions and episodic-history redaction remain uncovered. Task handoffs remain independent of rolling summaries.
8. Native Responses now supports text, images, function tools, SSE and encrypted continuation with local state. Per-model capability/effort configuration is implemented; extend live-model validation and automatic probing; hosted tools, WebSocket steering and API asynchronous tools remain unsupported.
9. Add fault-injection recovery evaluations and budgeted real-model reliability scenarios, with completion, false-completion, duplicate-effect, recovery and cost metrics. Deterministic checks alone do not establish real-world reliability.

## Validation

Run the repository's full `just check` gate after integrated changes. Use local mock services for delivery and crash boundaries; do not contact configured external endpoints while testing. Real-model evaluations require an explicit configured evaluation profile and cost cap, and must report actual sample counts and costs. The authorized ¥10 smoke used the existing `ds` profile; reported-use peak-price estimate across four invocations was ¥0.198044, with total conservative reservations ¥3.819316.

Attention-state recovery now checks exact run lineage even after the heartbeat is cleared, including admission of new chat turns. Foreground exec timeouts, output-read failures and termination without an exit code also trigger the structured uncertainty stop; killing a process does not roll back prior effects.

External input waiting now spans protocol, TS/Python SDKs and cockpit: immutable condition, idempotent targeted event, draft checkpoint, same-run recovery and persistent human pause priority (migrations 0035/0036). Human continuation atomically commits authorization, task activation and the reopened boundary. Source fields provide correlation and still require an authenticated host.

Operation journal and human reconciliation now span loop checkpoints, migration 0037, authenticated host methods, generated TS/Python clients and cockpit controls. Intent/results and checkpoint commits are atomic; immutable human evidence repairs an exact unresolved tool-result boundary without executing or resuming. Human receipts do not satisfy observed-tool success. Stopped pre-journal checkpoints now reconstruct explicitly labelled operation metadata without changing their conversation or starting work. Provider-supported idempotency keys and remote domain verification remain explicit follow-ups.

File acceptance now fingerprints the observed bytes and atomically commits versioned verification receipts with the checks projection. Completion receipts bind exact-run verification; corrupted receipt content/time is rejected on read. Evidence persistence failures stop completion. This covers file versions, not domain truth or remote read-back verification.

Common production startup now takes a kernel-held database execution lock before migrations/recovery. Server clones and background job settlement retain ownership; cross-process crash and competing-open tests cover release/rejection. This does not serialize writes to shared workspaces across sessions.

Memory observations atomically record hashed chat derivation contexts and deduplicate repeated extraction confirmations (0039). Human validity windows filter fact recall; static pages with bounded, missing or corrected sources are excluded at read time (0040). Existing untracked human pages cannot be overwritten. Episodic history, semantic paraphrase suppression and the semantic reliability of transient-state classification remain outside this guarantee.


Runtime mutation admission now serializes exclusive file/exec/declarative operations and unknown MCP effects across session registry copies. Background processes retain their permit through settlement; inspection and cancellation remain available. Busy admission is bounded and visible. This closes cooperating session/job overlap, not separate-home, human-editor or remote-service conflicts.


Main-model profiles now declare function-tool support, context-window estimates and Responses reasoning effort. Invalid declarations are rejected before credential writes; unsupported tool requests stop before provider calls. Native continuation remains lossless, and effort does not leak to a different small model. Automatic capability probing, hosted API tools and external-harness comparison remain separate follow-ups.

Structured document acceptance now compares typed JSON values at validated RFC6901 pointers with fixed expected values and bounded reads. Regression cases reject prose containing the expected number, string/number mismatches and missing-as-null confusion, and bind measurements to existing versioned receipts. This is local contract conformance, not external domain truth.

Expanded live evidence now covers explicit Chinese continuity and expired-memory recall.
The waiting/input/chat/delivery sample completed in one run with one final write; its retained
record required offline correction of a counter that also counted the draft. The earlier
four-step sample safely stopped for attention. All four live invocations total 45 requests,
peak-price estimate CNY 0.198044 and conservative reservations CNY 3.819316, within CNY 10.
See the live reliability report for raw failures, review provenance and unmeasured quality.

Ordinary-chat uncertainty now has human inspection/reconciliation, per-operation snapshot
bindings, immutable evidence and a same-session read-tool fence. New chat turns cannot bypass
unresolved effects, including unclaimed startup boundaries and legacy metadata gaps. Recording
evidence leaves the old turn stopped; a new instruction uses the current workspace. Task-owned
reconciliation keeps its exact latest-run fences. SDK and cockpit routes use the same host rules.

Real-process SIGKILL coverage now exercises pre-dispatch, committed-but-unacknowledged external effects and durable tool-result boundaries against an uninterrupted baseline (`just smoke-recovery`). Recovery charges known native checkpoint usage before stopping for uncertainty or missing configuration; repeated recovery uses the existing round watermark and preserves human pause. Uncheckpointed provider usage and active wall time remain approximate.

Quick chat now reads bounded saved personal preferences without extra provider calls, alongside same-session recent context and SOUL. The historical source label is preserved with an explicit uncertainty warning; it is not proof of human review. Deleted, corrected, bounded and inferred records are filtered appropriately. Emotional/relationship support is directed toward the full route in the classifier prompt, pending live quality validation.

Source-bound temporary observations now retain exact user-role quotations, uncalibrated model confidence and source-time expiry (0041). Typed extraction separates facts/states and preserves malformed results for retry. Durable source batches and episode context receipts (0042) recover interruption without merging new chats or duplicating the same batch summary. The memory worker retains runtime ownership and cancels/joins stalled calls during shutdown. Historical untracked claims, semantic classification quality and episodic redaction remain explicit limitations.

[Implementation status report (2026-11-14)](harness-implementation-report-2026-11-14.md)

The final pre-projection validation passed: 931 Rust tests, 113 frontend tests, 14 Python tests, deterministic 70/70, replay 5/5, four end-to-end suites and five process-recovery cases. Evidence is archived in `evals/results/harness-validation-2026-11-14-final.json` and `process-recovery-2026-11-14-final.json`. A new live extraction sample passed nickname-only durable retention and a source-bound six-hour fatigue observation. Five invocations now total 46 requests, peak-price estimate CNY 0.201684 and reservations CNY 3.891496. A subsequent projection refresh/retry/offline fix has seven focused worker tests passing and subsequently passed the complete integrated rerun: 935 Rust/226 app-server tests, all prior client/eval gates, four smokes and five recovery cases. See `harness-validation-2026-11-14-projection.json`.

Fixed-source research passed; the generic-write negative failure is retained; guarded publication passed both cases. Eight invocations total 72 requests, CNY 0.344474 estimated usage and CNY 6.274340 reservations. The adapter is evaluator-only; long research and support quality remain unvalidated. See the [review and migration/recovery guide](harness-release-review.md).
