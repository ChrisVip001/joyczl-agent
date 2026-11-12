# Harness upgrade implementation

English | [简体中文](harness-upgrade.zh.md)

Working implementation checklist for the 2026-11-12 architecture assessment. This is a continuation record, not a claim that the full assessment has been implemented. Preserve the Rust runtime, SQLite state, human-controlled authorization and generated protocol.

## Implemented and covered by regression tests

- Checkpoint execution intent before tool batches; failed checkpoint persistence prevents execution. Unknown in-flight batches are suspended rather than automatically replayed.
- Keep loop-round identity through checkpoints and continuation. Atomically charge task/run totals from cumulative round watermarks; accounting failure stops the task. Legacy checkpoints with already-accounted runs require review.
- Recover a task through its checkpoint's run identity. Fresh and recovered tasks share one accounting, budget, acceptance and goal driver; recovered goals can continue within the authorized round cap.
- Allocate background-job identities independently of process counters. Persist intent before launching a process; failure to write the ledger prevents launch.
- Drain process output before publishing job completion, with a bounded wait and explicit incomplete-output marker. Bind jobs to exact host task/run identities. Commit job outcomes, task timeline events and requested-delivery receipts together.
- Persist exact job dependency sets and resume authorized tasks after known completion, including restart. Human pause/cancel wins; interrupted or killed jobs require review. Competing wake workers claim once.
- Claim follow-up instructions atomically and preserve them until the outcome is recorded. Restarted unknown claims are retained for attention and reported in chat; concurrent drains cannot take the same instruction.
- Discover Chinese skills through distinct adjacent Han-character pairs while retaining ASCII matching and explicit-only restrictions.
- Include session-local recent messages and SOUL in quick replies; keep other sessions out.
- Reconnect failed MCP transports without replaying unknown calls. A structured uncertainty signal stops remaining tools and model continuation; the pending intent remains suspended for reconciliation.
- Preserve operation numbering across goal rounds so later operations are not silently deduplicated.
- Commit whole immutable plan versions with their readable steps; restore them for fresh/recovered tasks and subsequent goal rounds.
- Scope successful-tool acceptance to the current run. Truncated or iteration-limited responses cannot finish tasks.
- Preserve native Responses encrypted continuation and DeepSeek thinking continuation.
- Run a budgeted real-model smoke using profile `ds` / `deepseek-flash`: three confirmed scenarios, with raw evidence and costs in [live reliability](live-reliability.md).
- Preserve usable recovery paths in short tool-output previews.
- Persist configured completion notices before sending, retain failures for periodic/restart retry, and delete only after acknowledgement. Approval prompts are not replayed. Transport errors do not expose webhook credentials.

## Remaining work, in order

1. Extend round accounting fault tests to crash boundaries between observation, accounting and finalization. Active wall time before an uncheckpointed crash remains approximate; it is not a hard real-time cap.
2. Extend the restored plan snapshot into a structured handoff of constraints, evidence and next actions. The shared task driver covers fresh and recovered acceptance/goal rounds; ordinary chat auto-continuation still has separate pre-driver handling.
3. Extend durable waits beyond background jobs, and close the outcome-to-notification production gap for goal and schedule outcomes. Background-job completion already commits its event atomically and supports durable wake-up.
4. Add operation-level state and reconciliation for external side effects, including metadata for safe replay and provider-supported idempotency keys. Batch checkpoints remain conservative protection, not exactly-once external execution.
5. Strengthen acceptance with versioned artifact evidence and domain receipts. Separate generated output from verified completion; Current-run scoping already prevents old successful tool counts from satisfying new runs.
6. Give background tasks independent execution contexts while keeping one conversational persona and progress projection. Quick replies currently have bounded recent context, not full long-term-memory continuity.
7. Persist structured task handoffs, constraints and unresolved operations separately from rolling summaries. Preserve correction/deletion semantics in personal memory.
8. Native Responses now supports text, images, function tools, SSE and encrypted continuation with local state. Add explicit per-model capability/effort configuration and broader live-model validation; hosted tools, WebSocket steering and API asynchronous tools remain unsupported.
9. Add fault-injection recovery evaluations and budgeted real-model reliability scenarios, with completion, false-completion, duplicate-effect, recovery and cost metrics. Deterministic checks alone do not establish real-world reliability.

## Validation

Run the repository's full `just check` gate after integrated changes. Use local mock services for delivery and crash boundaries; do not contact configured external endpoints while testing. Real-model evaluations require an explicit configured evaluation profile and cost cap, and must report actual sample counts and costs. The authorized ¥10 smoke used the existing `ds` profile; reported-use peak-price estimate across two runs was ¥0.063274, with total conservative reservations ¥1.523568.
