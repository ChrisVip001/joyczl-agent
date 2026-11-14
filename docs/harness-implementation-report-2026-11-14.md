# Joy harness implementation report

English | [简体中文](harness-implementation-report-2026-11-14.zh.md)

Date: 2026-11-14. Scope: work pursued in this conversation since the 2026-11-12 architecture review.

**The reliability foundation and a synthetic delivery workflow have been substantially implemented. The original report is not fully delivered. Final integrated and end-to-end validation after the latest memory changes now passes. Release review, real domain workflows and quality validation remain incomplete; passing gates do not establish generally reliable unattended personal assistance.**

## Implemented capabilities

The initial architecture review, priorities and exit criteria were delivered, and CodeGraph was initialized as authorized. Implementation retained the existing technology foundation.

| Area | Implemented behavior and limits |
|---|---|
| Effects and recovery | Persist tool intent before execution, journal individual operations, stop on unknown outcomes, and bind human reconciliation to exact operations/checkpoints. Human receipts do not count as observed tool success. Arbitrary remote effects still have no exactly-once guarantee. |
| Task advancement | Share fresh/recovered task driving, accounting, acceptance and finalization; enforce task/run/turn identity; persist round identities and cumulative usage watermarks. |
| Accounting on stops | Charge known checkpoint usage even when uncertainty or missing configuration prevents continuation. Repeated recovery does not charge it twice. Unreceived or uncheckpointed provider usage remains unknown. |
| Independent tasks | Human-started durable queue, one worker, independent transcripts and session-scoped progress. Models cannot grant themselves execution authority. |
| Waiting and waking | Associate background jobs with tasks, declare external input conditions, target and deduplicate signals, resume authorized work, and preserve human pause priority. Correlation fields do not authenticate external events. |
| Completion and delivery | Atomically commit terminal status/receipts, retain outbox records and retry failures, and record schedule settlement/delivery. Redelivery does not rerun the model. Real delivery-channel reliability is not established. |
| Acceptance | Version and hash observed file bytes, bind verification to the exact run, compare typed JSON values at pointers, and label unverified production. This does not verify domain truth or remote results. |
| Ownership and conflicts | Hold canonical-database kernel ownership across runtime/background work; coordinate exclusive tool mutations across sessions. This is not distributed isolation or protection against separate homes, human editors or remote concurrency. |
| Ordinary chat uncertainty | Human inspection/reconciliation methods, generated clients and dashboard controls; fence subsequent same-session mutations. Reconciliation leaves the old turn stopped; fresh instructions use the current workspace. |
| Handoffs | Rebuild structured handoffs from persistent contracts, plans, steps and artifacts; retain whole immutable plan versions; stop on corrupt or foreign state. |
| Model adaptation | Native Responses text/images/function tools/SSE/encrypted continuation, DeepSeek reasoning continuation, and declared tool/context/effort capabilities. Reject unsupported requests locally. Automatic probing, hosted tools, WebSocket steering and asynchronous API tools remain unsupported. |
| Chat continuity | Bounded recent session context, SOUL and saved personal preferences for quick replies; Chinese skill discovery; classifier instructions favor the full route for emotional/relationship support. No extra quick-reply provider call. Historical source=user entries may be tool-written, and support quality is unmeasured. |
| Memory corrections | Suppression/correction lineage, extraction-context digests, validity windows, recall filtering, source checks for static pages, and protection of unmanaged human files. Semantic paraphrases and historical redaction remain incomplete. |
| Temporary observations | Separate durable facts from temporary states, validate user-role quotation/source digests, retain uncalibrated model confidence, and anchor 1–24 hour expiry to source time. Retry does not renew a claim; human edits prevail. Quotation matching does not verify meaning. |
| Extraction lifecycle | Persist admission/completion separately, recover original source batches without merging new chats, transact backoff, deduplicate episode summaries by source context, retain worker execution ownership, and cancel/join stalled calls on shutdown. Legacy untracked claims cannot be identified automatically. |
| Engineering artifacts | Extend migrations through 0042, generated protocol artifacts, the TS client, dashboard controls, regression/process tests and bilingual documentation. Final integrated validation passes; change packaging remains unfinished. |

See [implementation tracking](harness-upgrade.md) and [protocol contracts](protocol.md).

## Observed validation

| Check | Result and interpretation |
|---|---|
| Latest full `just check` | Exit 0, 939 Rust tests passed, zero failures, zero Clippy warnings; includes observation persistence and explicit stopped-task continuation fixes. |
| Latest deterministic/replay evaluations | 70/70 and 5/5 observed passing; scripted behavior is not general live-model reliability. |
| Key crates | State 89, memory 70, app-server 227 and evaluator 7 passed within the final workspace run. |
| Clients | Zero schema drift, 113 frontend tests passed in final validation. |
| Real process death | Five local-fixture cases observed passing: baseline, pre-dispatch, effect committed without reply, durable tool result, and admitted memory extraction. No tool replay observed. Unknown stops retained 15 known task tokens; baseline/result recovery retained 30. The memory case recovered the same two pending source rows, not a completed extraction. |
| Four end-to-end suites | App-server, gateway and dashboard all reran successfully after the final memory changes, using local fixtures. |
| Browser component checks | Reconciliation input preservation, snapshot binding, operation selection, keyboard focus and narrow-screen layout verified. This was not a complete product usability audit. |
| Earlier full-check failure | The disk-exhaustion attempt failed; a subsequent full rerun now passed. The failed attempt is not counted as a pass. |

The complete rerun finished with sufficient disk space. Retained evidence: [final validation metadata and source hashes](../evals/results/harness-validation-2026-11-14-projection.json), [five-case raw process report](../evals/results/process-recovery-2026-11-14-projection.json). The [earlier four-case report](../evals/results/process-recovery-2026-11-14.json) remains historical evidence. The memory case recovered two pending source rows; it does not claim completed extraction. `just smoke-recovery` reproduces the current script.

## Live-model evidence and costs

The authorized existing profile was **ds / DeepSeek / deepseek-flash**. This is a requested alias, not proof of a pinned internal backend revision.

The latest shared-grounding update passed the complete `just check` gate: 937 Rust, 113 frontend, 70 deterministic and five replay tests, plus workspace Clippy; eight example tests passed separately. See the [current validation record](../evals/results/grounding-validation-2026-11-14.json). An earlier run's WeChat worker did not exit; its 12 tests passed independently and the full retry passed after stopping that worker. No gateway source changed. The four end-to-end and five recovery cases above belong to the earlier projection baseline and were not repeated for this change.

Twelve invocations made 90 logical model requests: 112,803 reported input tokens and 20,172 output tokens. The historical peak-price estimate is **CNY 0.386982**; cumulative conservative reservations were **CNY 7.573676**, within the authorized **CNY 10**. Reservations are not actual spend, and the estimate is not invoice reconciliation.

Small synthetic samples covered continuity, Chinese continuity, file delivery, uncertainty stops, expired memory, and waiting/input/chat/delivery. Retained failures include an incorrect status assertion, an insufficient step limit, and a draft counted as duplicate final delivery. The last issue was resolved by an offline review of retained receipts, not an additional successful model run.

One new live structural extraction sample passed nickname-only durable retention and source-bound temporary fatigue. It does not score emotional support; quick preferences and extraction recovery have no new live quality score. Independent support quality scores, long research quality and real cross-day unattended reliability remain unmeasured. See [raw evidence, review provenance and rates](live-reliability.md).


Three new research invocations are retained. The first positive run passed. An unavailable-source run using generic file writes failed: task acceptance kept `needs_attention`, but the model still created a partial final file. A constrained publication adapter then passed both cases: independent reads, exact citations and typed facts gate the final write; a missing source leaves no final artifact. The original failure remains. This MCP domain adapter lives in the evaluator, not the production generic file-tool contract; missing-source explanation quality was not scored. See the [research report](public-source-research-2026-11-14.md) and [guarded raw cases](../evals/results/deepseek-flash-2026-11-14-research-guarded.json).

New Chinese support replies have a [source-hashed evidence review](../evals/results/analysis/support-review-2026-11-14.json). The DSML failure is retained; empty-tool capability notes and the format metric were corrected. The earlier structurally valid follow-up still makes a problematic memory claim. New samples with shared grounding acknowledge missing history and retain literal continuity, but unsupported scene details and availability wording remain; overall support quality is not established.

## Remaining work

| Priority | Outstanding work | Exit condition |
|---|---|---|
| P0 | Reviewable change/release packaging | Separate pre-existing working-tree changes, prepare independently reviewable commits/PR and migration/recovery guidance. This turn did not commit, merge or publish. |
| P1 | Domain evidence and remote read-back | Define and validate one concrete transaction's authentication, source, acceptance, read-back and reconciliation contract. |
| P1 | Service idempotency/event adapters | Integrate supported idempotency keys and authenticated events/receipts for concrete services; no universal exactly-once claim. |
| P1 | Real long-running delivery workflows | Measure research/citation quality, long waits, shutdowns, cross-day recovery, authorization changes and notification failures. Current coverage is mainly synthetic/offline. |
| P2 | Basic everyday conversation | Retain natural communication, grounded recall and honest commitments as supporting capabilities; complex-task execution, recovery and delivery take priority. |
| P1 | Memory quality/history governance | Validate classification, address paraphrase resurrection, historical unbounded emotions, pre-migration deletions/claims, and episode/backup redaction. Confidence is not calibrated. |
| P2 | Resource scopes | Add concrete path/resource conflict handling; separate homes, human edits and remote concurrency need their own contracts. |
| P2 | Models and external harnesses | Broaden live compatibility/probing and API capabilities; no same-tool/same-budget comparison with Codex, Claude Code, pi or another external harness has been performed. |
| P2 | Repeatable improvement evidence | Repeat a stable real task set and compare completion, false completion, duplicate effects, recovery and cost. Existing samples cannot establish general improvement. |

Against the original phases: **A implementation and regression gates passed; failed-round accounting is corrected, while release review and broader live quality remain open; B has synthetic delivery and a completed fixed-source live research pilot, but lacks long research and external transaction validation; C has adaptation/memory foundations but lacks quality validation and external-backend comparison.**

Final validation, the fixed-source research pilot and archival are complete. A [review/recovery guide](harness-release-review.md) is available; the user has committed module splitting, while the current follow-up fixes remain uncommitted. Next prioritize multi-step task execution, interrupted/resumed delivery, longer research and concrete transaction read-back. Everyday conversation remains a supporting capability. External harness comparison should use a stable common task/tool contract. Rust, SQLite, local-first state and the single protocol were retained. Distributed scheduling, large multi-agent systems and wholesale pluginization were intentionally deferred in the original report.

## Tool and attribution limits

This report uses implementation records, retained evaluation files and observed tool results. Other working-tree changes are not automatically attributed to this work.

A CodeGraph query did not execute because automatic approval review hit an account usage limit; the tool explicitly stated this was not a safety determination. The tool subsequently became unavailable. This is separate from the DeepSeek evaluation cost allowance.

## Long-task reliability follow-up

Complex task delivery is the product priority; everyday emotional conversation is a supporting capability. Required step or artifact persistence failures now preserve known spend and stop before acceptance or another model round. Evidence can be recorded idempotently after storage repair without repeating effects. Explicit human continuation of a stopped checkpoint now works; cancellation, stale-run and unknown-effect guards remain in place.

The final full gate passed: 939 Rust, 113 frontend, 70 deterministic and five replay tests, with zero Clippy warnings. The local scripted long-task benchmark changed from 8/11 (recovery 0/3) to 11/11 (recovery 3/3), after adding explicit human resume to fixtures and fixing stopped-checkpoint eligibility. Original acceptance assertions and the initial failure record are preserved; this is not a real-model capability improvement measurement. Five real SIGKILL scenarios also passed, separately from the benchmark’s in-process runtime recreation. No paid model calls were added. See the [validation and source hashes](../evals/results/task-reliability-validation-2026-11-14.json). Longer real research, cross-day transactions and independent quality review remain pending.

## Actual staged-research study

The [six-source staged study](long-research-study-2026-11-14.md) has begun, without a comprehensive-quality pass. The initial run and separate-context model review failed with network response decoding errors. A retained repeat produced a draft and suspended one run without a final file, but one of six literal citations failed; source audit found additional scope and retry-semantics errors. A failed round also left known checkpoint usage out of the task aggregate; the 2026-11-15 correction below addresses that defect. Actual next-day continuation is not eligible until 2026-11-15 22:12:55 Asia/Shanghai and will be human-triggered. The remaining conservative paid allowance is CNY 0.850620.

## 2026-11-15 failed-round accounting correction

Failure settlement now charges recorded checkpoint tokens through the existing cumulative watermark. A regression reproduces zero task tokens versus 28 known checkpoint tokens before the fix, then verifies exact charging, repeated settlement and storage-failure review without rerunning the model or tool. Original failed live records remain unchanged; missing provider usage is still unknown. Actual next-day research delivery remains pending.
