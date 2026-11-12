# Budgeted live reliability smoke

English | [简体中文](live-reliability.zh.md)

Run on 2026-11-13 with the existing `ds` profile: provider `deepseek`, model alias `deepseek-flash`. These are synthetic, isolated smoke samples, not production reliability estimates. No personal chat history, external search, messages or real transactions were used.

## Results

| Scenario | Confirmed result | Evidence |
|---|---|---|
| Chinese conversation continuity | Pass | Recalled Momo and the presentation concern across two turns |
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
