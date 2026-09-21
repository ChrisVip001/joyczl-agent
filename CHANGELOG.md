# Changelog

English | [简体中文](CHANGELOG.zh.md)

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
