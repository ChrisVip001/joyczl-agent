# Harness change review and recovery guide

English | [简体中文](harness-release-review.zh.md)

This is a review map, not a claim that every current working-tree change belongs to the harness work. No commit, pull request, deployment or release was created. The shared working tree contains unrelated changes; path membership alone cannot establish authorship. Preserve those changes when splitting commits.

| Review unit | Primary paths | Required evidence |
|---|---|---|
| Durable task/effect contract | `joyczl-protocol/src/protocol/v2.rs`, app-server task/resume/dispatch modules, loop checkpoints, state tasks/operations/completions | Checkpoint accounting, intent-before-effect, exact-run acceptance, five-case process recovery |
| Human control and clients | app-server dispatch/task controls, protocol exports/schema, TS client/dashboard task controls, generated Python models | Schema drift, same-session fences, client tests, four end-to-end smokes |
| Memory correction and lifetime | state fact edits/observations/validity/transient modules; memory extraction/retrieval/bundle modules | Source identity, exact quotations, source-time expiry, human precedence, no duplicate renewal |
| Consolidation and projections | state `chat_claims.rs`/episodes, memory consolidation, app-server `memory_worker.rs` and recovery tests | Stable recovered batches, atomic backoff, episode tombstones, ownership/cancellation, content-change refresh and failed-write retry |
| Research pilot | eval `examples/live_reliability.rs` and its modules, `scripts/test-research-publication.py`, dated result JSON and report | Independent source receipts, positive and unavailable-source outcomes, preserved failures, shared paid budget |
| Integration and explanations | `scripts/smoke-recovery.py`, bilingual architecture/protocol/operations/testing/reports | Matching behavior documentation and raw evidence/source hashes |

Rust paths above are relative to `joy-rs/`; inspect actual diffs, references and generated artifacts before moving a file into a review unit. Cross-unit dependencies mean these are review sections, not independently deployable commits. Protocol and generated clients must travel together. Database migrations through 0042 require the complete ordered migration set; do not cherry-pick only the latest migration.

## Upgrade and recovery

1. Stop all Joy runtimes and workers that own the target database. Confirm they have exited before making a filesystem backup. Do not copy only a live SQLite main file while a WAL may contain committed data; use an SQLite-aware backup if stopping is impossible.
2. Preserve a pre-upgrade snapshot of the complete home and configuration with restrictive permissions. It can include conversations, OAuth tokens and credentials; keep it out of source control and reports. Record the matching binary revision. Back up external workspaces separately when their files matter.
3. Test the new binary against a disposable copy with execution, delegation, hooks and external services disabled. Confirm migration/startup, memory views, task inspection and shutdown before starting authorized work. Do not start two runtimes against the same canonical database.
4. An unknown effect is a reconciliation boundary. Inspect the operation and external system, record a human decision using the provided control methods, and issue a fresh instruction where required. Restoring state does not undo an external effect and can lose its receipt; do not blindly retry it.
5. To return to an older binary, stop the new runtime and preserve its current state first. Restore the matching pre-upgrade snapshot into a separate home and use the corresponding binary. Do not run an older binary against a migrated database or manually drop new columns/tables. Reconcile effects and deliveries made after the snapshot before resuming anything.

There is no tested automatic schema downgrade, transactional rollback of remote effects or guarantee that a restored snapshot contains the latest task authorization. Current recovery tests cover crashes of the upgraded runtime, not an end-to-end old-version downgrade. The new extraction admission markers do not identify historical untracked claims. Memory deletion/correction is not full historical or backup redaction.

## Validation boundary

[Production validation](../evals/results/harness-validation-2026-11-14-projection.json): 935 Rust tests, 113 frontend tests, 14 Python tests, 70 deterministic and five replay cases; four end-to-end smokes and five process-recovery cases passed. These results precede subsequent evaluator-only research changes. The research example has its own seven-test/Clippy checks and live records. No production runtime source changed to introduce the research publication tool; it is a constrained MCP adapter in the opt-in evaluator.

The [implementation report](harness-implementation-report-2026-11-14.md) records remaining quality and integration work. Publication review still requires checking the mixed diff and actual candidate revision; a green gate is not a release authorization or proof of personal-assistant quality.
