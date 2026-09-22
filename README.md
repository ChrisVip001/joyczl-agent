English | [简体中文](README.zh.md)

# Joyczl-agent

> A local-first personal assistant: Rust owns logic and state, TypeScript
> covers the browser and chat gateways, Python stays a thin SDK. The state
> store, reasoning, and tools all run on the user's own machine. Codename
> **Joy** — the assistant's persona is Joy too.

## Design principles

1. **Rust owns logic and state.** The agent loop, memory, tools, and
   state.db live in Rust; crates are split by responsibility (target <500
   lines per module), with no `joy-core` mega-crate.
2. **The protocol has one definition.** `joyczl-protocol` is the single
   source of truth for the cross-language contract: types generate the
   TypeScript and Python bindings, and the artifacts are checked in.
3. **Clients only translate.** The CLI, dashboard, chat gateways, and Python
   SDK are all clients of `joy app-server` (JSON-RPC over stdio); state.db is
   opened by exactly one process.

## Quick start

```bash
cd joy-rs && cargo build
JOY_PROVIDER=deepseek DEEPSEEK_API_KEY=… ./target/debug/joy       # terminal REPL
JOY_PROVIDER=deepseek DEEPSEEK_API_KEY=… ./target/debug/joy dashboard   # → http://localhost:7777
```

A turn over raw protocol:

```bash
ANTHROPIC_API_KEY=sk-… JOY_HOME=/tmp/joy-demo ./target/debug/joy app-server <<'JSONRPC'
{"jsonrpc":"2.0","id":1,"method":"turn/start","params":{"message":"Remember that Alex likes morning meetings"}}
JSONRPC
```

The response follows a notification stream: `turnStarted → gateDecided →
textDelta* → toolStarted → toolCompleted* → turnCompleted`. `turnCompleted.meta`
records the gate ruling, tool timing, which model answered, and token usage —
persisted with the conversation.

## What Joy can do

- **Converse** on the terminal, in the dashboard, or through chat gateways
  (Telegram, Discord, WeChat, Lark — see [docs/gateways.md](docs/gateways.md))
- **Remember**: semantic facts (FTS5, CJK-capable), dated episodes, and
  procedural Skills — with a retrieval gate so memory is consulted only when
  relevant ([docs/architecture.md](docs/architecture.md))
- **Act**: 11 built-in tools plus MCP servers
  ([docs/configuration.md](docs/configuration.md))
- **Plan as a graph**: the triage front door answers small talk cheaply; the
  gather workflow writes a morning brief
  ([docs/architecture.md](docs/architecture.md))
- **Be examined**: deterministic evals gate every release; a judge grades
  answer quality ([docs/testing.md](docs/testing.md))
- **Be observed**: every turn lands a trace line and a usage row
  ([docs/architecture.md](docs/architecture.md))

### Built-in tools

| Tool | Purpose |
|---|---|
| `save_note` / `forget_note` / `search_memory` / `list_memory` / `manage_memory` | memory CRUD |
| `create_event` / `list_events` | calendar (idempotent, ICS + optional Apple Calendar sync) |
| `send_message` | message drafts into the outbox — never actually sends |
| `search_web` | DuckDuckGo HTML, or Tavily with `TAVILY_API_KEY` |
| `create_skill` | persist an agreed workflow as procedural memory |
| `current_time` | local time with weekday and timezone |
| `run_command` | shell commands — sandboxed, allowlisted, off unless `JOY_EXEC=1` ([SECURITY.md](SECURITY.md)) |
| `delegate_task` | hand one self-contained job to a subagent — off unless `JOY_DELEGATE=1`, cannot delegate again; pass `result_schema` for a structured answer (retried once, then prose) |

Tool failures return to the model as text — the loop never crashes on one.

## Documentation

| Document | Content |
|---|---|
| [docs/architecture.md](docs/architecture.md) | principles, crate dependency graph, one turn's data flow, key mechanisms, design decisions |
| [docs/protocol.md](docs/protocol.md) | the 13 JSON-RPC methods, notification sequence, error codes, clients |
| [docs/configuration.md](docs/configuration.md) | every `JOY_*` variable, gateway variables, mcp.json, settings.json |
| [docs/gateways.md](docs/gateways.md) | Telegram / Discord / WeChat / Lark setup, quirks, and security notes |
| [docs/testing.md](docs/testing.md) | the four-layer quality system and the latest test report |
| [docs/operations.md](docs/operations.md) | running every shape, backups, troubleshooting |
| [docs/limitations.md](docs/limitations.md) | every known boundary — deliberate or not — with the file to open next |
| [docs/internals/](docs/internals/README.md) | the tutorial: every crate's algorithms, data flow and invariants, chapter by chapter |
| [docs/skills.md](docs/skills.md) | skill authoring: format, triggers, install and export |
| [CONTRIBUTING.md](CONTRIBUTING.md) | engineering discipline and commit rules |
| [SECURITY.md](SECURITY.md) | the security model: credentials, capability boundaries, OAuth |
| [CHANGELOG.md](CHANGELOG.md) | changelog |

## Roadmap

| Phase | Content |
|---|---|
| P0 ✅ | protocol layer + generation pipeline (63 types) |
| P1 ✅ | state (SQLite+FTS5), app-server (JSON-RPC over stdio), `joy` CLI |
| P2 ✅ | config / provider (12, incl. Ollama) / tools / loop / memory (gate + consolidation) |
| P2b ✅ | provider streaming, MCP, graph + triage front door |
| P3 ✅ | dashboard, Telegram / Discord / WeChat / Lark gateways (the latter three never connected to real platforms) |
| P4 ✅ | terminal REPL, turn/interrupt, config/write + model/list, full tool set, Skills, trace/usage, gather, MCP OAuth, evals + gate + judge |
| P5 ✅ | local inference (Ollama, no key), `joy mcp serve` (memory as an MCP server), Homebrew formula + Dockerfile |
| P6 ✅ | sandboxed execution behind `JOY_EXEC` (deny list + allowlist + seatbelt/bwrap), context compaction (rolling summaries), declarative schedules (`joy schedule`) |
| P7 ✅ | hybrid retrieval (`JOY_EMBEDDINGS`, RRF), temporary-statement filter, skill updates (`joy skill update`) |
| P8 ✅ | startup config validation, tool-argument schema checks, a stall guard, token budgets, sandboxed-and-offline execution, visible retries, output spilling, subagents, interactive approval, skill policy fields, memory kinds |
| Next | distribution: PyPI wheel + npm shim; both unpublished |

## Naming

| Purpose | Name |
|---|---|
| Repo / PyPI / crates.io | `joyczl-agent` |
| Rust crate prefix | `joyczl-` |
| CLI binary | `joy` |
| Persona | Joy |
| Env vars | `JOY_*` |
| State directory | `.joy/` |

English | [简体中文](README.zh.md) · Code is MIT.
