# Testing strategy and test report

English | [简体中文](testing.zh.md)

Joy's quality system has four layers, cheapest first; **deterministic evals
passing 100% is the release gate** — one failure blocks the release.

## Layers

| Layer | Location | Shape | Latest full run |
|---|---|---|---|
| Unit tests | inside each joy-rs crate | in-process, offline, deterministic | 201 passed / 15 suites / 0 failed |
| Protocol generation | scripts/write_schema.py --check | artifact drift check | 0 drift |
| Frontend | joy-ts (node --test + tsc) | 99 passed / 0 failed | |
| Python SDK | sdk/python (pytest + mypy) | 13 passed / 0 failed; mypy clean on 8 files | |
| End-to-end smokes | scripts/smoke*.sh | real app-server / gateways / dashboard / SDK | 4/4 pass |
| Deterministic evals | evals/deterministic/*.jsonl | scripted model driving the same run_turn | 19/19 pass, gate exit 0 |
| Judge | evals/judge/*.jsonl | real model answers, referee grades (0-10) | scores reported, not blocking |

## Unit test distribution (per crate)

| Crate | Cases | Covers |
|---|---|---|
| joyczl-state | 17 | SQL contract, FTS5 search (CJK and junk input), calendar idempotence, memory-backend conformance |
| joyczl-tools | 19 | tool behavior and output wording (honest destinations), create_event idempotence, SKILL.md validation, parameter errors as text, the three execution gates, and that the sandbox really does block a write outside the allowed roots |
| joyczl-provider | 17 | both wire formats, SSE parsing, 429 rate limiting, model metadata, local inference (a provider that needs no key) |
| joyczl-loop | 10 | both guard exits, tool round-trip, streaming delta order, interrupt shutdown |
| joyczl-graph | 24 | wave execution, routing, collision detection, on_error draining, full triage/gather paths |
| joyczl-mcp | 30 | transport frames, handshake, tool registration, full OAuth flow against a local fake authorization server, token storage 0600, and the memory server it exposes (`joy mcp serve`) |
| joyczl-memory | 33 | gate fail-open, consolidation (including the temporary-statement filter), Skills triggering/rescan, MEMORY.md mirror, compaction waterline and fallback, RRF fusion, skill install/update |
| joyczl-app-server | 19 | protocol dispatch, interrupt, config/write persistence, model/list, trace/usage persistence |
| joyczl-cli | 7 | cron semantics, once-per-minute firing, job loading from both sources, and one firing that lands in the outbox |
| joyczl-config | 10 | env-var semantics, patch merge/save/clear |
| joyczl-protocol | 7 | RPC envelope serialization (the export test runs on demand) |
| joyczl-ops | 8 | the HTTP translation layer |

## Test report (full run, 2026-09-21)

| Suite | Result |
|---|---|
| cargo test --workspace (15 binary suites) | **201 passed / 0 failed** |
| clippy --workspace --all-targets -D warnings | 0 warnings |
| cargo fmt --check | pass |
| generated-artifact drift check | consistent with the Rust protocol definition |
| joy-ts typecheck + node --test | typecheck pass; **99 passed / 0 failed** |
| Python SDK mypy + pytest | mypy clean; **13 passed / 0 failed** |
| scripts/smoke.sh (app-server end to end) | pass |
| scripts/smoke-gateway.sh (four gateway stubbed chains) | pass |
| scripts/smoke-dashboard.sh (real dashboard HTTP/SSE) | pass |
| scripts/smoke-sdk-python.sh (SDK subprocess lifecycle) | pass |
| joy eval (deterministic evals, release gate) | **19/19, exit 0** (one sandbox scenario reports as skipped on machines without a usable sandbox) |
| joy judge (deepseek-v4-pro live) | 2 cases scored; the referee caught one "claimed saved but save_note never ran" fabrication (0/10) |

## What deterministic evals can assert

Scenarios are JSONL (one case per line) supporting: multi-turn scripts,
scripted gate and model responses (text / tool_use), reply contains/excludes,
**ordered tool-call sequences**, **tool output must contain** (distinguishing
"called" from "succeeded"), prompt contains/excludes (what the harness showed
the model), consolidation counts, interrupt timing, config/write hot reload, per-scenario
settings overrides (history window, execution policy, graph workflows) and
`prereq`-based skipping. Every assertion is offline and reproducible.

## Reproducing

```bash
just check            # fmt + schema drift + ts-check + py-check + clippy + test + eval
just eval             # deterministic evals only (exit 0 = releasable)
just judge            # judge scores (needs an API key)
just smoke            # app-server end to end
just smoke-gateway / smoke-dashboard / smoke-sdk-python
```

Failure policy: unit test and deterministic eval failures **block merging**;
judge scores only record trends; smoke failures block releases.
