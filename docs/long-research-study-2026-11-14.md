# Staged research and actual next-day continuation

English | [简体中文](long-research-study-2026-11-14.zh.md)

This opt-in study evaluates a useful decision report: how Joy should combine persistence, retries, human authorization and remote reconciliation for next-day work. The product focus is task delivery. Six curated official source excerpts from three publishers cover activity idempotence, workflow replay, retry policy, durable timers, local database atomicity and HTTP method semantics. Source discovery is performed by the evaluator; the tested model reads retained excerpts. This does not measure autonomous search or hours of uninterrupted reasoning.

[Preregistered criteria](../evals/results/long-research-preregistration-2026-11-14.json) precede model execution. The first phase produces six traceable claims, a decision matrix, limitations and proposed failure tests in `draft.json`. An existing `task/wait` condition suspends the task before final delivery. The separate `review` phase uses the same authorized model in another session with no tools; its complete criticism is retained. This provides context separation, not an independent human or different-model review. A nonempty review is never a semantic quality pass.

The `resume` phase refuses to run before 86,400 real seconds have elapsed. It runs in a new process on the retained store, checks the task is suspended, and submits the explicit human-triggered review signal twice to check duplicate acknowledgement. The original task then reads its draft and criticism and produces `report.md`. Task status, run identities, operation receipts, source citations and exact output bytes are retained. Final semantic review must check claim entailment, scope, counterarguments, corrections and decision usefulness; file existence alone is insufficient.

No real calendar, email or other account transaction is authorized or exercised. The host publishes the next-day input only when the user asks to continue. Nothing schedules an automatic continuation. The 24-hour gate is evaluation code, not a new production scheduler guarantee. Do not modify timestamps, source manifests or task state to force a success; preserve failed runs and use a new study home for independent repeats.

## Reproduce

The work home must be an absolute, isolated persistent directory containing `sources.json` with six distinct HTTPS source IDs, URLs and captured excerpts. Retain source bytes, retrieval times and hashes. Never use the user's normal Joy state as the work home. The configured profile home is used only to resolve the authorized model; no credentials are copied into the work home.

```sh
cargo run -p joyczl-eval --example live_reliability --manifest-path joy-rs/Cargo.toml -- \
  --home /path/to/configured/profile --suite research-long --phase start \
  --work-home /absolute/path/to/isolated/study --budget-cny 1.05 --out /path/to/new-start.json
```

Run again with `--phase review` and a new output path and an allowance within the remaining shared budget. After an actual 24-hour wait, explicitly run `--phase resume` with another new output path. Recheck official pricing and the cumulative [budget record](../evals/results/live-budget-2026-11-14.json) before any paid invocation. Each phase spends from the original CNY 10 authorization; the invocation cap is not a fresh overall budget. Existing suites gain no implicit requests. Preflight failures occur before resolving the paid provider. Preserve the complete output even when the case fails; the process exit code alone is not a pass verdict.

## Current evidence

The actual first-day outcome, review, eligible continuation time and validation hashes are recorded below after execution. Actual next-day recovery and final research quality remain pending until observed.

Three paid first-day phases retain their original failure verdicts:

- [Initial attempt](../evals/results/deepseek-flash-2026-11-14-long-research-start.json): network response decoding failed; failed task, no draft or final, one request missing usage.
- [Isolated repeat](../evals/results/deepseek-flash-2026-11-14-long-research-repeat.json): a draft and one suspended task run, no premature final; five of six literal quotes match. The timer quotation does not, so first-day acceptance fails.
- [Separate-context model review](../evals/results/deepseek-flash-2026-11-14-long-research-review.json): network response decoding failed, one request missing usage, no review file. Independent review is not complete.

The [coding assistant source audit](../evals/results/analysis/long-research-review-2026-11-14.json) requires revision: completed-activity replay versus retry is conflated; hosted-only scope is unsupported; the timer quotation is altered; the database boundary is too narrowly stated; and HTTP retry exceptions are incomplete. This is not blind or independent human review. The original draft is unchanged, and the audit does not stand in for a successful model-review invocation.

The initial failure also exposed a [task accounting gap](../evals/results/analysis/long-research-accounting-gap-2026-11-14.json): its checkpoint retains 7,341 input and 468 output tokens while the task aggregate reports zero. The independent evaluator retains reservations for every request, protecting the paid study budget. The production aggregate defect is unresolved; failure-path accounting cannot be declared complete.

An [actual early-resume invocation](../evals/results/long-research-early-resume-2026-11-14.json) was rejected before provider configuration resolution. The retained home is `/Users/chirslarn/AI-Agent/joy-namespace/long-research-2026-11-14-repeat`, including source pages, excerpt manifest, task state, draft, host audit and a first-day database snapshot. The original failed home is retained separately.

The earliest human continuation is **2026-11-15 22:12:55 Asia/Shanghai**, 24 actual hours after the repeat started. The missing separate-context review still needs completion before continuation. Final delivery must correct major audit issues and inspect run and operation receipts. Cross-day success remains pending. These three phases reserved CNY 1.575704; cumulatively 15 invocations made 101 requests, two without usage. The known-usage peak-price estimate is CNY 0.509160, reservations total CNY 9.149380, and CNY 0.850620 remains. These are not invoices; missing usage is not free and future calls must stay within the remaining allowance.

Initial live calls used the existing 30-second request cap. Future research phases use 60 seconds, with no automatic retries and the same overall budget. A timeout cause for the network errors is unproven; the configuration adjustment is not a verified fix or a successful rerun.

[Validation, operation receipts and source hashes](../evals/results/long-research-validation-2026-11-14.json): full `just check` passed (939 Rust, 113 frontend, 70 deterministic, five replay), plus 11 example tests and example Clippy. These are evaluator/runtime checks, not a research-quality pass.

## 2026-11-15 continuation

At about 03:01 Asia/Shanghai, fewer than five hours had elapsed since the retained draft started. The actual 24-hour continuation gate remains in effect; no paid review or task signal was sent. Work continues on the observed failed-round accounting defect. Desktop and session-permission work belongs to another concurrent thread.
