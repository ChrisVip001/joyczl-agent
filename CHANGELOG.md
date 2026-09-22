# Changelog

English | [简体中文](CHANGELOG.zh.md)

## 0.7.0 — Aligning with the state of the art

### Per-turn tool-result budget

History had a sliding window and a token budget; the turn itself did not, and MCP tools
can return anything. `JOY_TOOL_RESULT_TOTAL_CHARS` / `JOY_TOOL_RESULT_MAX_CHARS` now
replace the largest results with stubs — full text spilled, head/tail in complete lines,
tools that budget themselves skipped, idempotent, and a failed spill never turns a
successful call into an error. The defaults are deliberately large (200k / 30k).

### Subagent reports are transcriptions, and can be structured

A report carries a header saying it is the subagent's own account, lines imitating a
role prefix or our own control markers get a backslash, and the model is told how many
were defused. It does not judge maliciousness and does not touch permission checks —
the gates do that. `delegate_task` also takes an optional `result_schema`: the answer
must be JSON matching it, one retry carries the validation error back to the child, and
a second failure falls back to prose with the reason attached.

### Lifecycle hooks (`JOY_HOOKS`)

Twelve events (`PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionRequest`,
`SessionStart`, `SessionEnd`, `Stop`, `StopFailure`, `SubagentStart`, `SubagentStop`,
`PreCompact`, `PostCompact`) can run your shell commands from `<home>/hooks.json`:
exit 2 blocks, a JSON decision can rewrite input, rewrite output or add context, and a
`PermissionRequest` hook can answer on the user's behalf. Policy events time out closed,
observation events open; a `hooks.json` changed while running is refused with a reason;
a blocking `Stop` re-runs the turn once. Shell handlers only, Unix-first — both
deliberate, both in limitations.

### Memories stop piling up

`facts` now carries a unique index on `(subject, lower(trim(content)))`, and
`Facts::add` returns `(row, was_new)`: a repeat hands back the existing row instead of
storing a second copy. Consolidation counts only genuinely new facts, and `save_note`
says "already remembered" rather than pretending to store it again. Migration 0008
collapses the duplicates that already existed before creating the index — a unique
index that cannot be created would break every later write.

### MCP: a broken server stops costing a timeout per turn

A connection carries a breaker (3 consecutive failures → 60s, isolated per server):
while it is open, calls return an explanation instead of knocking, and after the
cooldown one probe is allowed — a success clears the count. Tool names that collapse
into each other (`a`+`b_c` vs `a_b`+`c`) are de-duplicated with a suffix, and the rename
is printed because that name is what the model and the user see.
`ToolRegistry::register` refuses to replace an existing name instead of silently
swapping an implementation.

### The estimator is calibrated by measurement

`usage.input_tokens` was recorded but never fed back. The loop now returns the estimate
and the measurement for the *same* request, the app-server pairs them in
`session_context` (migration 0007), and the next turn's budget is corrected by their
ratio (clamped to 0.5–2.0, so one odd request cannot drag it off).

## 0.6.0 — Guardrails, budgets, and an offline sandbox

### Configuration is validated at startup

Out-of-range values used to silently become defaults. `Settings::validate` now
runs once after the environment and `settings.json` are merged and aborts with
the variable named. One bounds table (`joyczl-config::BOUNDS`) serves both the
startup path and `config/write`.

### Tool arguments are checked against their schema

Every tool already declared an `input_schema`; now it is compiled at
registration and enforced before the handler runs, so a bad call comes back as
`Error: 参数不符合 … 的 schema —— /subject：42 is not of type "string"` instead
of a hand-written message, and no half-applied side effect is left behind.

### A stall guard

A model looping on the same call (three identical calls in a row) or
alternating between a small set (`A,B,A,B`) now gets told, and byte-identical
long results are replaced with a reference stub. Interleaved loops need their
own test — they reset the consecutive counter — so both shapes are detected.
`TurnMeta.guard_hits` makes a stuck turn visible in traces and the dashboard.

### Context is budgeted in tokens

`ProviderInfo.context_window` plus tiktoken estimation decide when to compact:
turns are now a ceiling, tokens are the gate. A provider reporting a context
overflow is recognised (`ProviderError::ContextOverflow`) and the turn is
compacted and retried **once**. `JOY_CONTEXT_WINDOW` / `JOY_COMPACT_THRESHOLD`
tune it; validation rejects `JOY_MAX_TOKENS >= JOY_CONTEXT_WINDOW`.

### Rate limits and transient failures are retried, visibly

429/5xx/network hiccups are retried with exponential backoff (500ms up to 8s per
wait, 30s total, `JOY_LLM_RETRIES` default 2, `0` disables). Every retry emits a
`Retry` notification and `TurnMeta.retries`, so the wait is explained rather
than mysterious; the REPL prints a line and the dashboard shows a chip. A
provider that reports `Retry-After` is obeyed, within the caps. There is no
provider failover — that is a separate decision.

### Skills gain policy fields, memories gain kinds

A skill can now say `allow-model-invocation: false` ("do not summon me by
accident") and `dependencies: a, b`. Opted-out and dependency-broken skills do
not fire from keyword overlap; `$skill-name` in a message forces the body in
(and is stripped from what the model sees). A reference to something that does
not exist is a hint, not an error.

Facts carry a `kind` (`user` / `feedback` / `project` / `reference` / `fact`),
chosen by the summarizer and normalised on write, exposed through
`memory/search`, `memory/list` and the protocol's `Fact`. Failed consolidation
now backs off exponentially (1 min doubling to 1 hour) instead of retrying the
same broken rows every turn.

### Interactive approval (`JOY_APPROVAL=on-request`)

A command that the allowlist does not match can now be asked about instead of
flatly refused. The turn emits `approvalRequested` (tool, the command, why,
the deadline) and waits; `approval/respond` answers it; the REPL and the
dashboard both implement the answering side. Silence is a refusal: timeouts, a
late reply, a closed connection and having no UI at all all end in no run.
Only the allowlist gate is negotiable — the hard deny list and the sandbox
cannot be approved away. "Remember" writes the exact command into
`settings.json` and takes effect next start.

### Subagents (`JOY_DELEGATE`)

`delegate_task` hands one self-contained job to a subagent and brings back only
the conclusion, in a fresh context (empty history, no retrieval, no skills). It
gets the parent's tools **minus `delegate_task`**, so delegating again is not
refused but impossible; it cannot ask you anything; and its answers are capped
at 5 iterations (hard cap 10) and 2048 tokens. The conclusion is annotated with
the tools it used, failed calls included, so the parent can tell whether the
answer rests on something that failed. Off by default.

### Long command output is kept, not just truncated

Output past 8000 characters is still cut from what the model sees (context has
to be protected), but the full text is written to `<home>/spill/<date>/…` and
the path is reported, so it can be read back. `spill/` keeps seven days, pruned
at startup; a failed write falls back to plain truncation.

### Behaviour change: sandboxed commands are offline

`run_command` now runs with **no network** by default — seatbelt gets
`(deny network*)`, bubblewrap gets `--unshare-net`. Commands that need to
download something (a `cargo test` that fetches crates, say) must set
`JOY_EXEC_NETWORK=1`. Extra writable directories can be opened with
`JOY_EXEC_WRITABLE_ROOTS` (colon separated, each must be an existing absolute
directory) — a build cache is the typical case. The startup line reports both.

## 0.5.0 — Local, sandboxed, scheduled

- Local inference: an `ollama` provider — no key, no network, nothing leaves
  the machine. LM Studio / vLLM work through `JOY_BASE_URL` + `JOY_MODEL`
- `joy mcp serve`: Joy becomes an MCP server exposing five memory tools, so
  other agents on this machine share the same facts (memory only — nothing
  that lets another agent make Joy act)
- Sandboxed execution: `run_command` behind `JOY_EXEC`, off by default, gated
  three times — an unconfigurable hard deny list, an allowlist (empty = deny
  everything), and macOS `sandbox-exec` / Linux `bubblewrap` with writes
  confined to the working directory, the Joy home and temp. No usable sandbox
  means the command is refused, never run unsandboxed
- Context compaction: turns pushed out of the working-memory window fold into
  a rolling per-session summary (stored in state.db, never recomputed from
  scratch); a dead summarizer falls back to a deterministic excerpt
- `joy schedule`: declarative jobs from skill frontmatter (`schedule: 0 8 * * 1-5`)
  or `schedules.json`, five-field cron, one firing per minute, results written
  to the outbox
- Hybrid retrieval (`JOY_EMBEDDINGS`, keyword and vector legs fused by rank /
  RRF) + `joy memory reindex` for facts written before the switch was on
- Consolidation drops temporary statements instead of filing them as facts
- `joy skill update`: index-driven upgrades — validate, stage, back up, swap
  atomically, never downgrade
- Distribution: Dockerfile (multi-stage, non-root, bubblewrap installed) and a
  Homebrew formula template
- `docs/limitations.md`: every known boundary — deliberate or unfinished — with
  the file to open next

## 0.4.0 — Feature-complete surface

- Protocol: `turn/interrupt` (cancellation tokens + racing shutdown),
  `config/write` (settings.json persistence + hot reload), `model/list` all
  implemented; `ToolStarted` notification; `TurnMeta.usage` and
  `meta.interrupted` persisted
- CLI: terminal REPL (bare `joy`), `joy gather` morning brief,
  `joy mcp login`, `joy skill export/install/list`
- Tools: `search_web`, `create_event`/`list_events` (idempotent + ICS +
  optional Apple Calendar sync), `send_message` (outbox drafts),
  `manage_memory`, `create_skill`
- Memory: procedural Skills (progressive disclosure + keyword triggering),
  per-turn MEMORY.md mirror
- Observability: traces in `traces/<date>.jsonl`, usage ledger
  `usage.jsonl`
- Graph: the gather morning-brief workflow (four parallel scans, propose
  only, fail-open)
- MCP: browser OAuth (discovery / dynamic registration / PKCE / callback /
  token storage / refresh) alongside stdio + HTTP transports
- Evaluation: `joyczl-eval` — deterministic evals (13 scenarios), release
  gate (`just check` tail), judge
- Docs: architecture / configuration / protocol / testing / operations /
  skills / CONTRIBUTING / SECURITY (English + `.zh.md` twins)

## 0.3.0 — Dashboard and gateways

- `joyczl-ops`: axum + SSE dashboard backend; `@joy/dashboard` frontend
- Gateways: Telegram, Discord (WebSocket + heartbeat/reconnect), WeChat
  (three crypto modes + 4-second race + customer-service messages), Lark
  (long connection + pbbp2 frames + fragment reassembly)
- Sessions: `session/*` protocol methods and history paging

## 0.2.0 — Loop and orchestration

- `joyczl-provider`: 11 providers, Anthropic/OpenAI wire formats, SSE
  streaming
- `joyczl-loop`: observe → reason → act → repeat; tool errors become text
- `joyczl-memory`: retrieval gate (fail-open) + consolidation
- `joyczl-tools`: built-in tool registry; `joyczl-graph`: wave DAG engine +
  triage front door; `joyczl-mcp`: stdio + HTTP transports

## 0.1.0 — Foundation

- `joyczl-protocol`: single source of truth + TS/JSON Schema/pydantic
  generation pipeline
- `joyczl-state`: SQLite + FTS5(trigram) + migrations
- `joyczl-app-server`: JSON-RPC over stdio; `joy` CLI
