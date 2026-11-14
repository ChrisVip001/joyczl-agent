# Testing strategy and test report

English | [简体中文](testing.zh.md)

Joy's quality system has four layers, cheapest first; **deterministic evals
passing 100% is the release gate** — one failure blocks the release.

## Layers

| Layer | Location | Shape | Latest full run |
|---|---|---|---|
| Unit tests | inside each joy-rs crate | in-process, offline, deterministic | 939 passed / 0 failed |
| Coverage | cargo llvm-cov / coverage | a **trend floor**, not a target | Rust 65.9% lines (floor 60) / Python 63% (floor 60) |
| Protocol generation | scripts/write_schema.py --check | artifact drift check | 0 drift |
| Frontend | joy-ts (node --test + tsc) | 113 passed / 0 failed | |
| End-to-end smokes | scripts/smoke*.sh | real app-server / gateways / dashboard / SDK | 4/4 pass |
| Deterministic evals | evals/deterministic/*.jsonl | scripted model driving the same run_turn | 70/70 pass, gate exit 0 |
| Replay evals | evals/replay/*.jsonl | recorded dialogue + **expected event sequence** (compaction / approval timeout / streamed output / MCP reconnect / step-budget continuation) | 5/5 pass, also in the gate |
| Long-task benchmarks | evals/long/*.jsonl | same harness, asks "did autonomous work get better" | 11/11 reached (one with the planner on), interrupt recovery 3/3; **never blocks** |
| Judge | evals/judge/*.jsonl | real model answers, referee grades (0-10) | 13 cases; scores reported, not blocking |

Judge scores must be integers from 0 through 10; an invalid score fails judging instead of entering the average. Three Chinese support fixtures cover listening without unsolicited plans, relationship boundaries and honest uncertainty about personal history. These are single-turn criteria, not evidence of improved support quality; multi-turn human review remains outstanding. The 2026-11-14 score-boundary update passed all six evaluator library tests and all-target evaluator Clippy with zero warnings; the full-run counts above describe the earlier workspace baseline.

## Unit test distribution (per crate)

| Crate | Cases | Covers |
|---|---|---|
| joyczl-state | 89 | SQL contract, FTS5 search (CJK and junk input), calendar idempotence, memory-backend conformance, fact-kind normalisation, consolidation backoff, and the concurrent claim (two turns both due distill once) |
| joyczl-tools | 200 | tool behavior and output wording (honest destinations), create_event idempotence, SKILL.md validation, argument schema validation, the three execution gates and approval (including that the sandbox really blocks a write outside the allowed roots), output spilling, and a subagent's tool table lacking delegate_task; and this round's additions: the twelve hook events' blocking/rewriting/timeout split plus trust hashes, the todo list's three hard limits, the background job ring overflow and owner fence, read_file/list_dir pagination and re-read short-circuit, and fetch_url's SSRF refusals plus HTML-to-text |
| joyczl-provider | 42 | both wire formats, SSE parsing, 429 backoff retry against real HTTP, token estimation, local inference (a provider that needs no key) |
| joyczl-loop | 38 | both guard exits, tool round-trip, streaming delta order, interrupt shutdown |
| joyczl-graph | 31 | wave execution, routing, collision detection, on_error draining, full triage/gather paths |
| joyczl-mcp | 37 | transport frames, handshake, tool registration, full OAuth flow against a local fake authorization server, token storage 0600, and the memory server it exposes (`joy mcp serve`) |
| joyczl-memory | 70 | gate fail-open, consolidation (temporary-statement filter, kinds, failure backoff), Skills triggering/rescan, explicit `$skill` and missing dependencies, MEMORY.md mirror, compaction waterline and fallback, RRF fusion, skill install/update |
| joyczl-agent | 20 | the deterministic core: the three acceptance checks (exists / substring / successful-call count), tool claims versus filesystem facts (contradictions included), plan storage and its failure fallback, path containment (outside the workspace or no workspace at all = refuse) |
| joyczl-app-server | 227 | protocol dispatch, interrupt, config/write persistence, model/list, trace/usage persistence, the approval waiting table (including the 'asked before running' causality) and subagents; the tool-result budget and observed-prefill calibration, subagent report scanning and the two-stage `result_schema` acceptance, lifecycle hooks, todo injection, real-process background jobs (skipped without a sandbox) and the goal loop's continue/round-limit/judge-failure paths; and the task engine: tasks with no session, a false done needing a human, acceptance passing only on a real file, over-budget stopping before the spend, one run per attempt, restart reconciliation without replay, unique step keys across rounds, and interrupt ending the task loop immediately; plus the session gate: a second turn in the same session is refused with `SESSION_BUSY` before any side effect, `steer` lands at a round boundary, `follow-up` survives a restart, and an idle call says there is nothing to wait for |
| joyczl-config | 35 | env-var semantics, startup validation (out of range = exit), patch merge/save/clear (**no model or key fields left**); multi-profile storage, secret backends (memory/file — never the real keychain), switching and removal |
| joyczl-cli | 15 | cron semantics, once-per-minute firing, job loading from both sources, and one firing that lands in the outbox |
| joyczl-protocol | 7 | RPC envelope serialization (the export test runs on demand) |
| joyczl-ops | 72 | the HTTP translation layer |

## Test report (full run, 2026-09-23)

| Suite | Result |
|---|---|
| cargo test --workspace (15 binary suites) | **415 passed / 0 failed** |
| clippy --workspace --all-targets -D warnings | 0 warnings |
| cargo fmt --check | pass |
| generated-artifact drift check | consistent with the Rust protocol definition |
| joy-ts typecheck + node --test | typecheck pass; **99 passed / 0 failed** |
| scripts/smoke.sh (app-server end to end) | pass |
| scripts/smoke-gateway.sh (four gateway stubbed chains) | pass |
| scripts/smoke-dashboard.sh (real dashboard HTTP/SSE) | pass |
| scripts/smoke-sdk-python.sh (SDK subprocess lifecycle) | pass |
| joy eval (deterministic evals, release gate) | **42/42, exit 0** (the sandbox scenarios report as skipped on machines without a usable sandbox) |
| joy judge (deepseek-v4-pro live) | 2 cases scored; the referee caught one "claimed saved but save_note never ran" fabrication (0/10) |

## The seven CI jobs

| Job | Guards |
|---|---|
| Rust (fmt / clippy / test) | formatting, `-D warnings`, the full unit suite |
| Rust (coverage floor 60%) | `cargo llvm-cov --fail-under-lines 60` — threshold set from a measured baseline |
| Windows (build + tests, sandbox tests not applicable) | everything but the sandbox compiles and passes on Windows; the job name says the sandbox does not apply |
| Protocol artifacts (drift check) | generated artifacts match the Rust definitions |
| TypeScript (typecheck / test) | front-end types and tests |
| Release gate (deterministic evals + smoke) | every deterministic eval plus a real-process smoke |

Coverage is a **trend floor**, set from the measured baseline (Rust 65.9% → floor
60, Python 63% → floor 60): low but true. Setting it high only produces tests
written to pass the gate, which is worse than no gate.

## What deterministic evals can assert

Scenarios are JSONL (one case per line) supporting: multi-turn scripts,
scripted gate and model responses (text / tool_use — multi-round tasks mark each round's
gate with `{"gate": …}`), reply contains/excludes,
**ordered tool-call sequences**, **tool output must contain** (distinguishing
"called" from "succeeded"), prompt contains/excludes (what the harness showed
the model), consolidation counts, interrupt timing, config/write hot reload, per-scenario
settings overrides (history window, execution policy, graph workflows) and
`prereq`-based skipping. Every assertion is offline and reproducible.

`prompt_contains` reads what the model actually saw: the **stable prefix plus the
per-turn tail**, and the tool table's name + description. Half of that is exactly what
two newer case families depend on — `deterministic/tool-policy.jsonl` (a deferred schema is
absent, `find_tools` opens it, the next request has it) and
`deterministic/prompt-shape.jsonl` (thin vs full soul, catalog present but body absent).
Per-scenario settings grew the matching knobs: `soul_mode`, `tool_policy`,
`skill_index_tokens`, `skill_body_tokens`, `tool_result_keep_recent`, `context_warn_ratio`.

## Replay evals (`joy eval --replay`)

Deterministic evals ask "is this behavior still correct". Replay evals ask "**does this run still
produce the same sequence of events**". A file in `evals/replay/*.jsonl` is a recorded dialogue
(what the human said, what the model answered) plus `expect.events`; the loader turns it into the
same `Scenario`, so the runner and the assertions are shared — the only additions are the loader
and the event assertion. There is no second harness.

Events are matched as an **ordered subsequence**: every item in
`["turnStarted", "toolStarted:run_command", "toolDelta", "toolCompleted:run_command:ok",
"turnCompleted"]` must appear in order, but extra `textDelta` / `toolDelta` in between is fine
(exact matching would make "one more delta" a red light — noise, not a regression). A line whose
`expect.events` is empty is refused at load time: a replay that asserts nothing is just a
deterministic case in disguise.

Five cases, one boundary each: compaction (window and token gate hitting together), approval
timeout (nobody answers → refusal, and the turn still finishes), streamed output (`toolDelta`
before `toolCompleted`), MCP disconnect (rebuild transport → replay handshake → retry once), and
the step budget running out with the todo list still open (it buys a slice, **carrying** the
previous segment's steps — a `last_prompt_contains` that can only come from message history
testifies to that).
It is part of the release gate (`just check` runs `eval-replay`).

## Long-task benchmarks (P1)

`joy bench` runs `evals/long/*.jsonl`: the same scripted harness, but it asks a different
question — **did autonomous task work get better or worse**. Two numbers stay separate,
because mixing them hides exactly what is worth watching:

* **task success**: cases that reached the outcome their own file declares (all declare
  `done`, i.e. acceptance passed on facts);
* **interrupt recovery**: the subset tagged `recovery` where a round is really interrupted,
  the runtime is reopened in-process on the same home, and an explicit human continuation
  still finishes the task. Actual process death is covered separately by `smoke-recovery`.

It **records and never blocks** (exit 0 even on failures): the value is the trend, and making
it a gate would only breed cases written to make the number look good. The report lands in
`eval_report.json` plus a per-suite `eval_report_long.json` (so a benchmark run cannot
overwrite the gate's verdict); history stays in `eval_runs.jsonl` with a `suite` field.

The benchmarks already paid for themselves — two real defects, both fixed with regression tests:

* **step keys collided across rounds**: numbering restarted at 1 each round, so `<turn_id>:<seq>`
  collided, round 2's write was silently dropped, and `toolsOk` never counted past one;
* **interrupts kept burning rounds**: the interrupted round's acceptance failed, "keep trying"
  bought another round, and the user's stop only took effect when the round cap ran out.

## Reproducing

```bash
just check            # fmt + schema drift + ts-check + py-check + clippy + test + eval
just eval             # deterministic evals only (exit 0 = releasable)
just eval-replay      # replay evals: recorded dialogue + expected event sequence
just bench            # long-task benchmarks (records, never blocks)
just judge            # judge scores (needs an API key)
just smoke            # app-server end to end
just smoke-gateway / smoke-dashboard / smoke-sdk-python
```

Failure policy: unit test and deterministic eval failures **block merging**;
long-task benchmarks and judge scores only record trends; smoke failures block releases.

TypeScript unit tests use a 30-second test timeout. A stalled local HTTP adapter fails the check instead of holding the full gate indefinitely.

## Real-process crash recovery

`just smoke-recovery` starts isolated app-server processes with local scripted provider and MCP fixtures. It uses SIGKILL before tool dispatch, after an external fixture commit but before its reply, and after a durable tool-result checkpoint. Assertions cover dispatch/effect counts, exact run identity, round accounting and known checkpoint token usage against an uninterrupted baseline. Unknown outcomes stop for human review; known results resume without replay. The suite requires POSIX process groups and does not contact configured services.

Focused regression tests also cover repeated recovery, missing model configuration, charging failures, foreign run identities and persistent human pause. Known checkpoint usage is charged even when recovery stops. Provider usage never received or checkpointed, and active wall time before a crash, remain unmeasured.

Retained local result: [2026-11-14 process recovery](../evals/results/process-recovery-2026-11-14.json), four cases passed. Both uncertainty stops retained 15 known tokens; uninterrupted and known-result recovery each retained 30 tokens with one run and one committed effect. CI runs this suite in its Linux release gate.

Memory recovery regressions exercise interruption before completion, retained claims after startup recovery failure, source-set isolation from new chats, atomic backoff failure, and a failure after facts/episode commit but before source completion. The same source batch cannot inflate confirmations or duplicate its episode; a human-deleted episode is not resurrected. Worker tests also verify execution ownership and cancellation/join of a stalled model future. The process crash smoke includes a memory-admission SIGKILL case; its result distinguishes resumable source rows from completed extraction.

### Recovery fixture controls

`restart` reopens the runtime in-process; it does not kill an OS process. `resume_task: true` is a trusted fixture action calling the human-only `task/resume` API, with `message` as its control reason rather than a new chat turn. Continuation consumes the checkpoint without a fresh retrieval-gate script. Without this action the task stays paused; original artifact checks are unchanged. These fields are evaluator-only, not application protocol or model tools. The initial 8/11, recovery 0/3 report is retained; benchmark exit 0 never substitutes for inspecting case outcomes.
