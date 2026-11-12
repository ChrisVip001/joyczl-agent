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
                 joyczl-provider ────── 12 providers (incl. local Ollama), two wire formats + SSE
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
        joyczl-agent ────────── the deterministic core: acceptance checks, tool observation,
                │               execution plans, the one path-containment rule
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
  → [memory retrieval] facts (FTS5, top_k) + episodes → the prompt tail
  → [Skills] catalog every turn (name + one line); bodies only on a match
  → [prompt assembly] stable prefix (soul + model identity) + per-turn tail
      (clock · capability gaps · todo · memory hits · catalogs · summary)
      the prefix is byte-identical within a session: that is the cache boundary
  → [THE LOOP] observe → reason → act → repeat
        · every model call races the cancellation token (turn/interrupt)
        · every tool execution races it too; errors return as text
        · the last allowed step is a wrap-up: tools go away, the model summarizes
  → [ledger] every row appended (user / assistant / tool_use / tool_result);
      payloads over JOY_LEDGER_INLINE_MAX spool to <home>/ledger/
  → [persist] conversation + meta (gate/graph/tools/model/usage) into chat_log
  → [consolidation] every N turns, a small model distills facts + an episode
  → [bundle] the background worker writes those into <home>/memory/ and reindexes
  → [mirrors] MEMORY.md (human-readable), traces/<date>.jsonl, usage.jsonl
  → [notifications] turnStarted → gateDecided → textDelta → toolStarted →
           toolCompleted → consolidationCompleted → turnCompleted
```

## Key mechanisms

### State (joyczl-state)

One `state.db`: `facts` / `episodes` (each with an FTS5 trigram index and
sync triggers), `chat_log` + `sessions` (0018: a session has a row of its own — name and
working directory; the body is still `chat_log`), `calendar_events`
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
* **Hybrid retrieval**: keyword hits (FTS5) and vector neighbours are fused by
  rank (RRF), so "that launch plan we discussed" can find "ships October 15".
  Off by default; a dead embedding service degrades to keyword-only.
* **Consolidation**: every N turns, unconsolidated dialogue is distilled into
  facts (source-labelled) and one episode; failure never loses the raw log.
* **Compaction**: turns pushed out of the working-memory window are folded
  into a rolling per-session summary (stored in state.db, carried forward —
  never recomputed from scratch). A dead summarizer falls back to a
  deterministic excerpt, so eviction never means amnesia.
* **Skills (procedural memory)**: `SKILL.md` (Agent Skills format) with
  progressive disclosure — frontmatter is scanned every turn (cheap), the
  body enters the system prompt only when the message matches. `MEMORY.md` is
  a human-readable mirror rebuilt every turn; state.db remains the source of
  truth.
* **The bundle (derived knowledge)**: the background worker distils facts into
  `<home>/memory/` pages (OKF shape, one concept per file) and keeps an FTS5 index
  over them. The prompt carries the **catalog**, not the pages; the model pulls one
  with `read_memory_page`. A person may edit a page by hand — the next
  `joy memory reindex` takes the file as truth.
* **Ledger first**: every turn is appended to `state.db`'s `ledger` row by row
  (digest per row, oversized payloads spooled under `<home>/ledger/`), and the
  memory center reads turns back from it. None of the layers above is the only
  copy of anything.

### Context budget (what actually reaches the model)

Everything the model sees each turn is assembled once, in `build_system`, and then
measured by the memory center's meter:

| Section | Where it comes from | Knob |
|---|---|---|
| soul | `SOUL.md` (thin or full) | `JOY_SOUL` |
| memory | gate hits + the bundle catalog + the skill catalog | `JOY_RETRIEVAL_TOP_K` |
| skills | the bodies that matched this message | — |
| tools | JSON schemas of the registered tools | `JOY_TOOL_POLICY=deferred` shrinks it |
| history | the last N turns, read from the ledger | `JOY_HISTORY_TURNS` |
| summary | the rolling compaction summary | `JOY_COMPACT_THRESHOLD` |

Three things keep that budget from quietly growing: **compaction** folds evicted turns
into the rolling summary (deterministic excerpt fallback, so eviction is never amnesia);
**tool-result trimming** stubs the oldest oversized results before each request
(`JOY_TOOL_RESULT_TOTAL_CHARS`, newest `JOY_TOOL_RESULT_KEEP_RECENT` kept whole); and the
**iteration budget** bounds the turn itself (`JOY_MAX_ITERATIONS`; the last step is a
wrap-up). The short lines that ride in the tail — clock, todo board, capability note — are
a few dozen tokens and are deliberately **not** part of the six-section meter: the band
exists to show what grows, and constants only add noise.

### Tools (joyczl-tools)

Some two dozen built-in tools + MCP, merged into one `ToolRegistry`. The contract:
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

## Three layers of memory

* **The ledger is the fact.** `ledger` in `state.db`, append-only: each turn's
  user / assistant / tool_use / tool_result / image rows, every row with a `digest` over its
  canonical payload. Payloads over `JOY_LEDGER_INLINE_MAX` move to `<home>/ledger/…` and the
  row keeps a pointer, digest and byte count. Any turn can be rebuilt from it — the history
  loader reads turns back, it does not read a folded transcript. Rows that came through the
  one-off backfill carry `fidelity: folded`, which is the honest label for "this tool output
  only exists as a summary".
* **The bundle is derived knowledge.** `<home>/memory/` in OKF shape: one Markdown page per
  concept, frontmatter carrying `title` / `kind` / `sources`, `index.md` as the catalog,
  `changelog.md` as the history. A person can edit any page; when they do, the page's digest
  no longer matches the index and the next `joy memory reindex` takes the **file** as truth.
* **The index is a cache.** SQLite FTS5 behind the retrieval gate. Losing it costs a reindex,
  never a fact. `MEMORY.md` is a generated copy of the catalog, kept only for the transition.

The prompt carries the **catalog** of the first two layers — not the encyclopedia. A new
session starts with a directory and pulls pages on demand (`read_memory_page`), which is the
same progressive-disclosure move as the skill catalog and the deferred tool table: the model
sees a table of contents and decides, instead of being handed everything every turn.
