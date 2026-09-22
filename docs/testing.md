# Testing strategy and test report

English | [简体中文](testing.zh.md)

Joy's quality system has four layers, cheapest first; **deterministic evals
passing 100% is the release gate** — one failure blocks the release.

## Layers

| Layer | Location | Shape | Latest full run |
|---|---|---|---|
| Unit tests | inside each joy-rs crate | in-process, offline, deterministic | 255 passed / 15 suites / 0 failed |
| Coverage | cargo llvm-cov / coverage | a **trend floor**, not a target | Rust 65.9% lines (floor 60) / Python 63% (floor 60) |
| Protocol generation | scripts/write_schema.py --check | artifact drift check | 0 drift |
| Frontend | joy-ts (node --test + tsc) | 99 passed / 0 failed | |
| Python SDK | sdk/python (pytest + mypy) | 13 passed / 0 failed; mypy clean on 8 files | |
| End-to-end smokes | scripts/smoke*.sh | real app-server / gateways / dashboard / SDK | 4/4 pass |
| Deterministic evals | evals/deterministic/*.jsonl | scripted model driving the same run_turn | 30/30 pass, gate exit 0 |
| Judge | evals/judge/*.jsonl | real model answers, referee grades (0-10) | scores reported, not blocking |

## Unit test distribution (per crate)

| Crate | Cases | Covers |
|---|---|---|
| joyczl-state | 19 | SQL contract, FTS5 search (CJK and junk input), calendar idempotence, memory-backend conformance, fact-kind normalisation, consolidation backoff |
| joyczl-tools | 39 | tool behavior and output wording (honest destinations), create_event idempotence, SKILL.md validation, argument schema validation, the three execution gates and approval (including that the sandbox really blocks a write outside the allowed roots), output spilling, and a subagent's tool table lacking delegate_task |
| joyczl-provider | 24 | both wire formats, SSE parsing, 429 backoff retry against real HTTP, token estimation, local inference (a provider that needs no key) |
| joyczl-loop | 16 | both guard exits, tool round-trip, streaming delta order, interrupt shutdown |
| joyczl-graph | 24 | wave execution, routing, collision detection, on_error draining, full triage/gather paths |
| joyczl-mcp | 30 | transport frames, handshake, tool registration, full OAuth flow against a local fake authorization server, token storage 0600, and the memory server it exposes (`joy mcp serve`) |
| joyczl-memory | 38 | gate fail-open, consolidation (temporary-statement filter, kinds, failure backoff), Skills triggering/rescan, explicit `$skill` and missing dependencies, MEMORY.md mirror, compaction waterline and fallback, RRF fusion, skill install/update |
| joyczl-app-server | 25 | protocol dispatch, interrupt, config/write persistence, model/list, trace/usage persistence, the approval waiting table (including the 'asked before running' causality) and subagents |
| joyczl-config | 18 | env-var semantics, startup validation (out of range = exit), patch merge/save/clear |
| joyczl-cli | 7 | cron semantics, once-per-minute firing, job loading from both sources, and one firing that lands in the outbox |
| joyczl-protocol | 7 | RPC envelope serialization (the export test runs on demand) |
| joyczl-ops | 8 | the HTTP translation layer |

## Test report (full run, 2026-09-21)

| Suite | Result |
|---|---|
| cargo test --workspace (15 binary suites) | **255 passed / 0 failed** |
| clippy --workspace --all-targets -D warnings | 0 warnings |
| cargo fmt --check | pass |
| generated-artifact drift check | consistent with the Rust protocol definition |
| joy-ts typecheck + node --test | typecheck pass; **99 passed / 0 failed** |
| Python SDK mypy (strict) + pytest | mypy clean; **13 passed / 0 failed**; coverage 63% (floor 60) |
| scripts/smoke.sh (app-server end to end) | pass |
| scripts/smoke-gateway.sh (four gateway stubbed chains) | pass |
| scripts/smoke-dashboard.sh (real dashboard HTTP/SSE) | pass |
| scripts/smoke-sdk-python.sh (SDK subprocess lifecycle) | pass |
| joy eval (deterministic evals, release gate) | **30/30, exit 0** (the sandbox scenarios report as skipped on machines without a usable sandbox) |
| joy judge (deepseek-v4-pro live) | 2 cases scored; the referee caught one "claimed saved but save_note never ran" fabrication (0/10) |

## The seven CI jobs

| Job | Guards |
|---|---|
| Rust (fmt / clippy / test) | formatting, `-D warnings`, the full unit suite |
| Rust (coverage floor 60%) | `cargo llvm-cov --fail-under-lines 60` — threshold set from a measured baseline |
| Windows (build + tests, sandbox tests not applicable) | everything but the sandbox compiles and passes on Windows; the job name says the sandbox does not apply |
| Protocol artifacts (drift check) | generated artifacts match the Rust definitions |
| TypeScript (typecheck / test) | front-end types and tests |
| Python SDK (mypy strict / pytest + coverage floor 60%) | strict typing plus the coverage floor |
| Release gate (deterministic evals + smoke) | every deterministic eval plus a real-process smoke |

Coverage is a **trend floor**, set from the measured baseline (Rust 65.9% → floor
60, Python 63% → floor 60): low but true. Setting it high only produces tests
written to pass the gate, which is worse than no gate.

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
