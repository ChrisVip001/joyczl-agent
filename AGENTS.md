# AGENTS.md

English | [简体中文](AGENTS.zh.md)

Joy is a local-first personal assistant: Rust owns logic and state,
TypeScript covers the browser and chat gateways, and Python stays a thin SDK.
Read [docs/architecture.md](docs/architecture.md) before changing `joy-rs/`;
this file is the working standard for AI coding assistants and human
contributors alike.

## Repository layout

```
joy-rs/
  joyczl-protocol/      the cross-language contract, single source of truth
                        (schema/ artifacts are checked in)
  joyczl-config/        JOY_* environment variables, read once at boot
  joyczl-state/         SQLite + FTS5(trigram); memory-backend contract & conformance
  joyczl-provider/      12 providers (incl. local Ollama), two wire formats + SSE, retry + token estimation
  joyczl-tools/         tool registry (schema-checked) + built-in tools, sandboxed exec, subagents,
                        approval, lifecycle hooks, the todo list, the background-job host trait
  joyczl-loop/          the agent loop (cancellable)
  joyczl-graph/         wave-based DAG engine + triage / gather workflows
  joyczl-mcp/           MCP client (stdio/HTTP) + browser OAuth + memory server (`joy mcp serve`)
  joyczl-memory/        retrieval gate + consolidation + Skills + MEMORY.md mirror
  joyczl-app-server/    JSON-RPC over stdio, the only holder of state.db, and with it the whole turn:
                        approval, hooks, todo injection, background jobs, the goal loop
  joyczl-ops/           dashboard backend (axum + SSE)
  joyczl-cli/           the joy binary (REPL / dashboard / gather / eval / mcp / skill)
  joyczl-eval/          deterministic evals + judge + release gate
joy-ts/                 @joy/client, @joy/gateway, @joy/dashboard
sdk/python/             thin Python client (pydantic models are generated)
evals/                  deterministic and judge scenarios (JSONL)
docs/                   architecture / configuration / protocol / testing / operations / skills / limitations
                        internals/ — the implementation tutorial (one chapter per layer)
scripts/                schema generation + four end-to-end smokes
```

## Commands

```sh
just check                    # every gate: fmt + schema drift + ts-check + py-check
                              # + clippy -D warnings + cargo test + deterministic eval
just eval / just judge        # deterministic eval (exit 0 = releasable) / judge scores
just write-app-server-schema  # required after any protocol change;
                              # just check-app-server-schema is the CI drift check
just smoke smoke-gateway smoke-dashboard smoke-sdk-python   # end-to-end smokes
```

## Non-negotiable invariants

- **The protocol has one definition**: types live only in
  `joyczl-protocol/src/protocol/v2.rs`; after changing them run
  `just write-app-server-schema` and commit the artifacts. Wire fields are
  camelCase, integers are `i32`, new types go into the `export.rs` list.
- **Tool errors are text, not exceptions**: `ToolRegistry::execute` never
  returns Err.
- **Fail-open paths need tests**: the retrieval gate, triage, and gather
  failure paths matter as much as the success paths.
- **Zero clippy warnings** (`-D warnings`); target <500 lines per crate module.
- **Configuration comes from environment variables**; `config/write`
  patches are validated and rejected wholesale — invalid values never reach
  settings.json.
- **No heavy new dependencies**: check the workspace for an equivalent first
  (URL codecs and hashing already exist here).
- **Credentials live in environment variables only**: never in files,
  protocol payloads, or logs; OAuth tokens go to `mcp-auth/` (0600).
- **Propose, never act**: the gather graph stays tool-free; `send_message`
  never actually sends.
- **Extensions are injected**: the tool layer knows only traits (`SubagentRunner`,
  `ApprovalBroker`, `HookRunner`, `JobRegistry`); every implementation lives in
  app-server and `ToolCtx` only gains fields. The dependency points one way — the
  other way is a cycle.
- **Capabilities are switched on by humans**: `JOY_EXEC` / `JOY_DELEGATE` /
  `JOY_HOOKS` are off by default, and the paths only a human may start (approval,
  goals, memory writes) are not exposed to the model at all — no tool, no route.
  "The model assigning itself work" is not forbidden here, it is impossible.
- **Never silent**: retries, circuit breaking, skipped duplicate writes, goal
  rounds, finished background jobs and hooks refused by a trust hash each owe the
  user a stderr line or a protocol notification. Watching a frozen screen and
  guessing why is the class of problem this round kept fixing.

## Testing

Behavior changes come with tests: unit tests live inside each crate,
cross-module assembly behavior lives in `evals/deterministic/*.jsonl` (one
case per line; multi-turn, prompt assertions, interruption, and config
injection are supported). Reproduce a bug in a test before fixing it. A
failing `joy eval` blocks release.

## Docs

User-visible behavior changes update the README and the matching `docs/`
 booklet in the same change. Documentation is written in English as the
 primary language with a `.zh.md` Chinese twin per file; update both
 together. One fact per home, no control-flow narration.
