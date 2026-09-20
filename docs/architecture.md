# Architecture

English | [简体中文](architecture.zh.md)

Joy is a local-first personal assistant: state, reasoning, and tool execution
all happen on the user's own machine. This document describes the software
structure that makes that possible; running and configuring it live in
[operations.md](operations.md) and [configuration.md](configuration.md).

## Design principles

1. **Rust owns logic and state.** The agent loop, memory, tools, and the
   SQLite state store live in Rust; crates are split by responsibility
   (target: <500 lines per module) — there is no `joy-core` mega-crate.
2. **The protocol has one definition.** `joyczl-protocol` is the single
   source of truth for the cross-language contract: `#[derive(TS)]` emits
   TypeScript, `#[derive(JsonSchema)]` emits JSON Schema which then drives
   Python pydantic models, and the artifacts are checked in.
3. **Clients only translate.** The CLI, dashboard, chat gateways, and Python
   SDK are all clients of `joy app-server` (JSON-RPC over stdio). state.db is
   opened by exactly one process.

## Crate dependency graph

```
                 joyczl-protocol  ←──── single source of truth (TS/JSON Schema/pydantic artifacts)
                 joyczl-config    ←──── JOY_* env vars, read once at boot
                        │
                 joyczl-provider ────── 11 model providers, two wire formats + SSE
                        │
        ┌───────────────┼────────────────┐
   joyczl-tools    joyczl-loop       joyczl-mcp
   (tool registry) (THE LOOP)       (MCP client + OAuth)
        │               │
        └───────┬───────┘
           joyczl-memory          joyczl-graph
        (gate/consolidation/   (wave-based DAG engine +
         Skills)                triage/gather)
                │
         joyczl-state ────────── SQLite + FTS5(trigram), sqlx migrations
                │
        joyczl-app-server ──── JSON-RPC over stdio, the only holder of state.db
          │         │
     joyczl-cli  joyczl-ops (dashboard: axum + SSE)
          │
     joyczl-eval (deterministic eval / judge / release gate)
```

Dependencies point downward and never cycle. `joyczl-eval` drives the core
through the same public API (`Server`, `run_turn`, `install_provider`) as
every client — a daily proof of the architecture: whatever an eval can drive
black-box, any frontend can.

## Data flow of one turn

```
user message
  → [triage graph, optional] classify (small model) ∥ check_calendar → quick | full
  → [retrieval gate] a small model decides whether to consult memory
      (fail-open: a broken gate means search anyway)
  → [memory retrieval] facts (FTS5, top_k) + episodes → into the system prompt
  → [Skills] scan SKILL.md frontmatter; load the body only on a match
  → [THE LOOP] observe → reason → act → repeat
        · every model call races the cancellation token (turn/interrupt)
        · every tool execution races it too; errors return as text
  → [persist] conversation + meta (gate/graph/tools/model/usage) into chat_log
  → [consolidation] every N turns, a small model distills facts + an episode
  → [mirrors] MEMORY.md (human-readable), traces/<date>.jsonl, usage.jsonl
  → [notifications] turnStarted → gateDecided → textDelta → toolStarted →
           toolCompleted → consolidationCompleted → turnCompleted
```

## Key mechanisms

### State (joyczl-state)

One `state.db`: `facts` / `episodes` (each with an FTS5 trigram index and
sync triggers), `chat_log` (a session is a label column), `calendar_events`
((title, start) unique index provides SQL-level idempotence). Migrations are
embedded at compile time via `sqlx::migrate!` — the binary carries its own
schema. Chinese search works through trigram + a LIKE fallback. The memory
backend contract lives in `store.rs`: the `SemanticStore` / `EpisodicStore`
traits plus `conformance` verification; a second backend means implement the
interface and run the same tests.

### Memory decisions (joyczl-memory)

* **Retrieval gate**: a small model decides "does this message need memory?",
  answering JSON; any failure opens the gate — stale memory beats lost
  memory.
* **Consolidation**: every N turns, unconsolidated dialogue is distilled into
  facts (source-labelled) and one episode; failure never loses the raw log.
* **Skills (procedural memory)**: `SKILL.md` (Agent Skills format) with
  progressive disclosure — frontmatter is scanned every turn (cheap), the
  body enters the system prompt only when the message matches. `MEMORY.md` is
  a human-readable mirror rebuilt every turn; state.db remains the source of
  truth.

### Tools (joyczl-tools)

11 built-in tools + MCP, merged into one `ToolRegistry`. The contract:
`execute` never returns Err — errors return to the model as `Error: …` text
and the loop keeps going. A tool failure is information the model can correct,
not a crash.

### Graph engine (joyczl-graph)

A deterministic wave-executed DAG: state is a blackboard (parallel nodes
writing the same key = an engine-level error), routers are plain code
functions (control flow is never handed to the model), and each node carries
`max_visits` plus a global `max_steps` guard. Two workflows: **triage** (small
talk takes a small-model quick reply, everything else enters the full loop;
any failure falls back to the plain path) and **gather** (morning brief: four
parallel scans → one tool-free synthesis → count-based routing into a draft;
propose only, never act).

### Interruption (turn/interrupt)

Each turn registers a cancellation token; model calls and tool executions race
it via `tokio::select!`, and the losing future is dropped (the HTTP connection
closes with it). A partial reply is stored honestly with
`meta.interrupted = true`.

### Evaluation (joyczl-eval)

Deterministic evals drive the same `run_turn` in-process with a scripted
model: offline, 0/1, reproducible, scenarios in
`evals/deterministic/*.jsonl`. **100% pass = the release gate** (the exit code
of `joy eval`). The judge has a real model answer and a cheaper model grade
0-10 against a rubric — scores are reported, releases are not blocked by them.
Reports land in `eval_report.json`, history appends to `eval_runs.jsonl`.

## Security boundaries

API keys exist only in environment variables — never in files, protocol
payloads, or logs; MCP OAuth tokens land in `mcp-auth/` (0600, temp file then
atomic rename); `send_message` only drafts; the gather graph is structurally
tool-free; skill names are forced slugs against path traversal. The complete
list is in [SECURITY.md](../SECURITY.md).

## Design decisions

### Why Chinese search needed its own migration

FTS5's default `unicode61` tokenizer treats a run of Chinese as one token, so
"早上" never matches "Alex likes morning meetings". `migrations/0002` switches
to `trigram` (sliding three-character slices), which makes Chinese work
naturally; the shortest queries trigram cannot slice (two-character Chinese
words are common) fall back to a LIKE substring scan — a personal memory store
is a few thousand rows, one full-table LIKE is microseconds.

### Why generated artifacts are checked in

"Changed the Rust protocol, forgot to regenerate" is a class of bug that never
errors and only makes frontends read stale types silently. Committing the
artifacts puts that drift in the PR diff; CI gates it with `--check`.

### Why generation code compiles only under test

`joyczl-protocol`'s runtime dependencies are just `serde` and `serde_json` —
`ts-rs` and `schemars` are dev-dependencies, switched by
`joyczl-protocol-noop-macros`:

```rust
#[cfg(test)]      pub(crate) use ts_rs::TS;
#[cfg(not(test))] pub(crate) use joyczl_protocol_noop_macros::TS;   // empty macro
```

So `cargo build` never touches the generators and the binary carries none of
them.
