# Budgeted live reliability smoke

English | [简体中文](live-reliability.zh.md)

Run on 2026-11-13 with the existing `ds` profile: provider `deepseek`, model alias `deepseek-flash`. These are synthetic, isolated smoke samples, not production reliability estimates. No personal chat history, external search, messages or real transactions were used.

## Results

| Scenario | Confirmed result | Evidence |
|---|---|---|
| Conversation continuity | Pass | Recalled Momo and the presentation concern across two turns |
| Artifact delivery | Pass | Wrote the three requested items, read them back with a tool, passed file acceptance and reached `done` |
| Uncertain external effect | Pass | Local MCP server committed then disconnected; exactly one commit, task `needs_attention`, no replay or false completion |

Each scenario ran twice. The first run's uncertainty assertion incorrectly expected `needsAttention`; the runtime returned `needs_attention` and behaved correctly. The original failing evaluation record is preserved. After correcting the evaluator, all three confirmation samples passed. This is too small a sample to infer a general success rate. Restart recovery, long research quality, emotional-support quality and delivery reliability were not measured by this live suite; their offline regressions have separate coverage.

Two runs made 20 logical model requests with zero request failures lacking usage. Reported totals: 15,009 input and 4,157 output tokens. At official peak cache-miss rates (¥2/M input, ¥8/M output), the estimate is **¥0.063274**. The sum of conservative request reservations was **¥1.523568**, below the authorized total **¥10**. Cache discounts and off-peak discounts are ignored. This is a usage-based estimate, not an invoice. [Official pricing](https://api-docs.deepseek.com/zh-cn/quick_start/pricing/).

The suite limits each request to 2,048 output tokens, 64,000 serialized input bytes plus a reservation margin, 18 calls per run and no automatic provider retries. It reserves twice the peak-priced upper estimate before sending. All model calls, including retrieval gates, share the same limiter and model. Separate runs must share the user's overall budget: the first reservation was ¥0.761848 and the second run's cap was reduced to ¥9.

## Reproduce explicitly

```sh
cargo test -p joyczl-eval --example live_reliability --manifest-path joy-rs/Cargo.toml
cargo run -p joyczl-eval --example live_reliability --manifest-path joy-rs/Cargo.toml -- \
  --home /path/to/configured/joy-home --budget-cny 1 --out /tmp/joy-live.json
```

The live command contacts the configured official DeepSeek endpoint and spends money. It only accepts `deepseek-flash` with the known rate schedule; verify prices before later runs. Credentials are loaded in memory through the existing resolver. Every case boots its own temporary state and tool allowlist. The live suite is intentionally outside `just check`.

Raw evidence: [initial run](../evals/results/deepseek-flash-2026-11-13-initial.json), [confirmation](../evals/results/deepseek-flash-2026-11-13-confirmed.json). Temporary paths in these records describe disposable workspaces, not retained deliverables.

## Expanded lifecycle samples

The expanded run added explicit Chinese continuity, waiting/input delivery and expired-memory
recall. Chinese continuity and expired-memory recall passed. The first waiting sample exhausted
the evaluator's four-step limit and correctly stopped at `needs_attention`, without verified
completion. Its raw failed record is retained. A targeted repeat used eight steps, preserving
the token/request/cost caps, and reached `done` with typed JSON acceptance.

The repeat's original automated verdict was also false: its counter combined one draft write
(`meeting.draft.json`) and one final write (`meeting.json`). Retained operation receipts show
one of each, one run, no final artifact before input, independent chat, a duplicate event
acknowledgement and numeric `approved:42`. An offline source-hashed review confirms that
runtime outcome; the corrected evaluator now counts only the final target and has a regression
test. This is an evidence review, not an additional successful model rerun. The first signal
acknowledgement was not separately retained in that older record. Emotional-support quality,
long research quality and cross-day live recovery remain unmeasured.

Across all four invocations: **45 logical requests**, **58,534 input / 10,122 output tokens**,
no recorded request failures missing usage, **¥0.198044** peak-rate usage estimate and
**¥3.819316** conservative reservations, within the authorized **¥10** total. Prices were
rechecked before these calls; discounts are ignored. This remains an estimate, not an invoice.

Evidence: [expanded raw run](../evals/results/deepseek-flash-2026-11-13-expanded.json),
[targeted repeat](../evals/results/deepseek-flash-2026-11-13-wait-confirmed.json),
[offline review with source digest](../evals/results/analysis/wait-delivery-review.json).
Run the expanded suite explicitly with `--suite expanded`; optionally select
`--case waiting-chat-delivery`. Share the overall budget across invocations.

## 2026-11-14 temporary-state extraction sample

The opt-in `--suite memory` uses the same ds/deepseek-flash profile, isolated synthetic state
and a CNY 0.50 invocation cap. One request passed: only the nickname Xiaolin was retained as
a durable fact; reported fatigue had a matching quotation/source digest and six-hour expiry
anchored to the source message. The model reported confidence 0.9, which is not calibrated.
This is not an emotional-support score or a general semantic-classification success rate.
An offline counterexample verifies that adding a permanent mood still fails the metric even
when a temporary copy also exists.

This invocation reported 440 input/345 output tokens, peak-price estimate CNY 0.003640,
reservation CNY 0.072180 and no missing-usage request. Across five invocations: **46 requests,
58,974 input/10,467 output tokens, estimate CNY 0.201684, reservations CNY 3.891496**, within
the original CNY 10 authorization. Rates were rechecked on 2026-11-14; these are peak-price
estimates without discounts, not invoice reconciliation. [Official prices](https://api-docs.deepseek.com/zh-cn/quick_start/pricing/).

[Raw fixture/model text/database evidence](../evals/results/deepseek-flash-2026-11-14-memory.json),
[cumulative budget record](../evals/results/live-budget-2026-11-14.json). Add `--suite memory` to
the explicit reproduction command. It incurs fees; invocations share the total allowance.
The existing baseline/expanded suites do not gain implicit requests.

## 2026-11-14 fixed-source research pilot

`--suite research --case source-report` passed actual HTTPS reads of two supplied official
pages, independent receipts, typed facts and local JSON delivery. The separate
`--case source-unavailable` failed the no-final-file requirement: task status remained
`needs_attention`, but the model wrote a partial file. Final acceptance alone did not
prevent publication. Both raw records are retained; no failure was relabelled.

`--suite research-guarded` removes generic file writes and exposes a fixed-path MCP publish
tool. It requires both in-process observed reads, exact citations, actual boolean facts,
a short summary and idempotent output before atomic publication. Seven example tests and
Clippy passed, including missing/partial reads, wrong facts/types and overwrite rejection.
Both live cases passed: valid sources produced `done`; unavailable sources produced no file
and `needs_attention`. The failing source was requested repeatedly: fail-fast efficiency
and response explanation quality are not established by this metric. This adapter is only
in the opt-in evaluator, not a production generic-write policy or arbitrary synthesis judge.

Reproduce using the existing explicit home/profile command with `--suite research-guarded
--budget-cny 2 --out <new-report-path>`. Omit the newline inside the CLI arguments. It incurs
fees; preflight the cumulative allowance before every invocation and retain new raw records.
The bounded test supplies sources and expected facts; it does not benchmark autonomous
search, long research, cross-day operation or the external harness described by the pages.

Across **eight invocations: 72 requests, 105,485 input / 16,688 output tokens, peak-price
estimate CNY 0.344474, conservative reservations CNY 6.274340**, within the shared CNY 10 cap.
No recorded request lacked usage. These are estimates/reservations, not provider invoices.
The CLI launch that rejected an unregistered suite locally made no model request and is
not counted as a paid invocation. Prices were verified on 2026-11-14.

[Positive raw run](../evals/results/deepseek-flash-2026-11-14-research.json),
[retained negative failure](../evals/results/deepseek-flash-2026-11-14-research-unavailable.json),
[guarded positive/negative](../evals/results/deepseek-flash-2026-11-14-research-guarded.json),
[validation hashes](../evals/results/research-validation-2026-11-14.json),
[cumulative budget](../evals/results/live-budget-2026-11-14.json),
[human-readable report](public-source-research-2026-11-14.md).
