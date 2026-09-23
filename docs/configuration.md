# Configuration reference

English | [简体中文](configuration.zh.md)

The only source of configuration is environment variables: `joyczl-config`
reads them once at process start and never again. Joy **does not read any
`.env` file** — source one yourself before launching
(`set -a; source .env; set +a`). Runtime changes go through `config/write`
(persisted to `<home>/settings.json`, surviving restarts).

## Invalid values fail at startup

`Settings::validate` runs once, **after** the environment and
`<home>/settings.json` are merged, and a bad value aborts the process with the
variable named in the message — a misconfiguration should not quietly become a
default that only surfaces later as strange behaviour. The same bounds table
(`BOUNDS` in `joyczl-config`) validates `config/write` patches, so the two entry
points cannot disagree.

| Variable | Range |
|---|---|
| `JOY_MAX_ITERATIONS` | 1 – 100 |
| `JOY_MAX_TOKENS` | 128 – 200000 |
| `JOY_HISTORY_TURNS` | 0 – 1000 |
| `JOY_CONTEXT_WINDOW` | 1024 – 10000000, and must exceed `JOY_MAX_TOKENS` |
| `JOY_COMPACT_THRESHOLD` | 0.05 – 0.95 |
| `JOY_TOOL_RESULT_TOTAL_CHARS` | 0 – 4000000 |
| `JOY_TOOL_RESULT_MAX_CHARS` | 0 – 1000000 |
| `JOY_CONSOLIDATE_EVERY` | 1 – 1000 |
| `JOY_RETRIEVAL_TOP_K` | 1 – 100 |
| `JOY_LLM_TIMEOUT` | 1 – 3600 |
| `JOY_LLM_RETRIES` | 0 – 5 |
| `JOY_GOAL_MAX_ROUNDS` | 1 – 20 |
| `JOY_EXEC_TIMEOUT` | 1 – 3600 |

`JOY_EXEC_ALLOW` is checked too: at most 64 rules, each non-empty, no newlines,
at most 200 characters. An **empty** allowlist is valid — it means "allow
nothing", which is the default.

## Core variables

| Variable | Default | Purpose |
|---|---|---|
| `JOY_HOME` | `./.joy` | state directory: state.db, SOUL.md, skills/, traces/, outbox/, mcp.json, settings.json |
| `JOY_PROVIDER` | `anthropic` | provider: anthropic / openai / deepseek / gemini / kimi / glm / minimax / xai / openrouter / opencode_zen / opencode_go / ollama |
| `JOY_API_KEY` | — | explicit key, takes precedence over the provider's default variable |
| `JOY_BASE_URL` | provider default | override the API endpoint (tests can point it at a fake server) |
| `JOY_MODEL` / `JOY_SMALL_MODEL` | provider default | main model / cheap model (retrieval gate and consolidation) |
| `JOY_LLM_TIMEOUT` | `120` | per-call model timeout in seconds |
| `JOY_LLM_RETRIES` | `2` |
| `JOY_GOAL_MAX_ROUNDS` | `5` | how many rounds a goal may continue for; on overrun the status is `round-limit` and the miss is reported honestly | retries on 429/5xx/network hiccups — exponential backoff (500ms, capped at 8s per wait, 30s total), **announced every time**, never a different provider; `0` disables |

## Behavior knobs

| Variable | Default | Purpose |
|---|---|---|
| `JOY_MAX_ITERATIONS` | `10` | hard iteration cap for one loop |
| `JOY_MAX_TOKENS` | `8192` | per-call output cap (headroom for reasoning models) |
| `JOY_HISTORY_TURNS` | `12` | working-memory window **ceiling**: only the last N turns enter the prompt (older turns are folded into a rolling summary, not dropped) |
| `JOY_CONTEXT_WINDOW` | provider default | override the context-window estimate (local models differ wildly; the table holds common defaults) |
| `JOY_COMPACT_THRESHOLD` | `0.8` | start compacting at this fraction of the window — tokens are the real gate, turns are the ceiling |
| `JOY_TOOL_RESULT_TOTAL_CHARS` | `200000` | per-turn cap on the total size of tool results; over it the largest are replaced with stubs (`0` disables) |
| `JOY_TOOL_RESULT_MAX_CHARS` | `30000` | a single result must exceed this to be stubbed (`0` disables) |
| `JOY_CONSOLIDATE_EVERY` | `6` | run consolidation every N new turns |
| `JOY_RETRIEVAL_TOP_K` | `4` | facts fetched when the gate opens |
| `JOY_GRAPH_WORKFLOWS` | `0` | enable the triage front-door graph (fail-open, costs time only) |
| `JOY_APPLE_CALENDAR` | `0` | sync `create_event` to Calendar.app via AppleScript |
| `JOY_SKILL_DIRS` | — | colon-separated extra skill directories (besides `home/skills`) |
| `JOY_JUDGE_MODEL` | small model | referee model for `joy judge` (the referee is not a contestant) |
| `JOY_GH_REPO` | — | repository for gather's GitHub scan (owner/repo) |

## Hybrid retrieval

| Variable | Default | Purpose |
|---|---|---|
| `JOY_EMBEDDINGS` | `0` | fuse vector similarity into memory retrieval |
| `JOY_EMBED_MODEL` | — | embedding model (e.g. `nomic-embed-text` for Ollama) |

Off by default: keyword (FTS5) retrieval works with no model and no network.
When on, results are fused by rank (RRF, k=60) rather than by score — bm25 and
cosine are not the same unit, and pretending otherwise is inventing data. A
dead embedding service degrades to keyword-only with a warning; it never
becomes "I remember nothing". Facts written before the switch was on have no
vector: run `joy memory reindex` to backfill.

## Lifecycle hooks (`JOY_HOOKS`)

With `JOY_HOOKS=1`, Joy reads `<home>/hooks.json` and runs **your shell commands** on
twelve events — the place to add behaviour without editing the loop: auditing, policy
gates, formatting, feeding tool calls to something else.

```json
{ "disableAllHooks": false,
  "hooks": {
    "PreToolUse": [
      { "matcher": "run_command", "timeout": 30, "command": "path/to/hook.sh", "args": [] }
    ],
    "PostToolUse": [ { "matcher": "*", "command": "audit.sh" } ] } }
```

**Events** (named as in Claude Code / codex): `PreToolUse`, `PostToolUse`,
`PostToolUseFailure`, `PermissionRequest`, `SessionStart`, `SessionEnd`, `Stop`,
`StopFailure`, `SubagentStart`, `SubagentStop`, `PreCompact`, `PostCompact`.

**What the command gets**: a JSON payload on stdin (`hook_event_name`, `session_id`,
`cwd`, `tool_name`, `tool_input`, `tool_output`, `matcher`, …).

**How it answers**:

| Exit / output | Meaning |
|---|---|
| `exit 0` | allow; a JSON stdout is parsed as below |
| `exit 2` | **block** (the only exit code that blocks), reason from `reason` or stderr |
| any other code | a non-blocking error: one stderr line, the action proceeds — a broken hook must not disable every tool |
| `{"decision":"block","reason":"…"}` | same, via JSON |
| `{"updatedInput":{…}}` | rewrite tool input (re-validated afterwards; a bad rewrite says the hook did it) |
| `{"updatedOutput":"…"}` | rewrite the tool result |
| `{"additionalContext":"…"}` | add context for the model (`SessionStart` folds it into this turn) |
| `{"hookSpecificOutput":{"permissionDecision":"allow\|deny"}}` | answer a `PermissionRequest` on the user's behalf |

**Timeouts split two ways**: policy events (`PreToolUse`, `PermissionRequest`, `Stop`)
time out **closed** — a gate that cannot answer in time must not wave things through;
observation events time out **open**. Per-entry `timeout` wins, then
`JOY_HOOKS_TIMEOUT`, then the event default (30s for interaction-sensitive events,
600s otherwise).

**A changed file is not executed**: the content hash is recorded at load; if
`hooks.json` changes while running (another process writing it), the new content is
**not** run and stderr says so. Swapping a live policy hook is the kind of silent
change that should stop and be seen.

A blocking `Stop` re-runs the turn **once**: it is the ancestor of the goal loop
(`goal/set`), without the round cap, judge or human-authority boundary.

## Per-turn tool-result budget

History has a sliding window and a token budget; the turn itself did not. `run_command`
caps itself at 8000 characters and `search_web` at 400, but **MCP tools answer to
nobody** — ten calls returning 50k characters each put half a million characters into a
single request.

When the total size of a turn's tool results exceeds `JOY_TOOL_RESULT_TOTAL_CHARS`, the
largest results are replaced with a stub: the full text goes to `<home>/spill/<date>/`,
and the context keeps the head and tail — **complete lines only** — plus
`…（结果共 N 字符，已截断，省略了 M 行；完整输出在 spill/…）`.

* Only results above `JOY_TOOL_RESULT_MAX_CHARS` are touched. A hundred medium results
  overflowing the cap cost more in files than the context they save, so that case is
  logged on stderr and left alone.
* Never half a line — half a JSON object reads worse than one line fewer.
* A failed spill still stubs (without promising a path it does not have) and never turns
  a successful call into an error.
* Tools that budget themselves (`run_command`) are skipped; the pass is idempotent.

## Running commands

| Variable | Default | Purpose |
|---|---|---|
| `JOY_HOOKS` | `0` | read `<home>/hooks.json` and run your shell commands on twelve lifecycle events (below) |
| `JOY_HOOKS_TIMEOUT` | `30` | default hook timeout in seconds; a per-entry `timeout` wins |
| `JOY_EXEC` | `0` | enable the `run_command` tool (off = the model never sees it) |
| `JOY_DELEGATE` | `0` | enable the `delegate_task` tool (a subagent gets its own context; it cannot delegate again) |
| `JOY_EXEC_ALLOW` | — | allowlist, comma separated, trailing `*` wildcards (`cargo test,git status,ls *`). Empty = deny everything |
| `JOY_EXEC_TIMEOUT` | `30` | per-command timeout in seconds |
| `JOY_APPROVAL` | `never` | what to do when the allowlist does not match: `never` = refuse, `on-request` = ask (no answer = refusal). Never applies to the deny list or the sandbox |
| `JOY_APPROVAL_TIMEOUT` | `120` | seconds to wait for an answer before treating it as a refusal |
| `JOY_EXEC_NETWORK` | `0` | let sandboxed commands reach the network. **Off by default** — an allowed command should not be able to send your data out |
| `JOY_EXEC_WRITABLE_ROOTS` | — | extra writable directories (colon separated, must be existing absolute paths), e.g. a build cache |

Commands always run sandboxed (macOS `sandbox-exec` / Linux `bubblewrap`)
with writes confined to the working directory, the Joy home and temp, and
**no network** unless `JOY_EXEC_NETWORK=1`; a
machine without a sandbox refuses to run anything. The hard deny list
(`sudo`, `mkfs`, download-piped-into-shell, …) is not configurable. See
[SECURITY.md](../SECURITY.md).

## Approving a command interactively

By default an unmatched command is simply refused. With `JOY_APPROVAL=on-request`
it becomes a question: the turn emits `approvalRequested` (tool, the command
itself, why it was not allowed, the deadline) and **waits**. The REPL prints
`y`/`a`/anything-else, the dashboard renders a small confirm bar; both send
`approval/respond`. Silence is a refusal — timeouts, a late answer and a
missing UI all end the same way.

`remember: true` (the `a` answer, or "允许并记住") appends the command — exactly
as approved, no wildcard — to the exec allowlist in `settings.json`, which takes
effect on the next start.

## Delegating work to a subagent

`JOY_DELEGATE=1` adds `delegate_task`, which hands one self-contained job to a
subagent and brings back only the conclusion — useful when the work would
otherwise fill the conversation with searching and reading, or when you want it
done in a fresh context. What the subagent gets:

* an empty history and a short system prompt (soul + "do this one thing"),
  with no retrieval, no skills and no summary;
* the parent's tool table **minus `delegate_task`** — delegating again is not
  refused, it simply is not an option;
* its own iteration limit (default 5, hard cap 10) and at most 2048 output
  tokens;
* no interactive approval path and no way to ask you a question.

The conclusion comes back with the tools it used, and failed calls are marked,
so a parent can tell whether the answer rests on something that worked. The
subagent's conversation is not stored. It runs in the same process against the
same state, so nothing is isolated beyond its context.

## Local inference (Ollama)

`JOY_PROVIDER=ollama` runs entirely on this machine: no key, no network,
nothing leaves the computer. Install [Ollama](https://ollama.com), then:

```bash
ollama pull qwen3:8b     # the main model
ollama pull qwen3:4b     # the cheap model (retrieval gate, consolidation)
JOY_PROVIDER=ollama joy  # no API key anywhere
```

Ollama exposes an OpenAI-compatible endpoint (`http://127.0.0.1:11434/v1`),
so it reuses the same wire as the cloud providers. LM Studio and vLLM are
the same shape — point `JOY_BASE_URL` at them and `JOY_MODEL` at whatever
you have loaded. Override the defaults per model:

```bash
JOY_PROVIDER=ollama JOY_MODEL=qwen3:14b JOY_SMALL_MODEL=qwen3:4b joy
```

This is the privacy path and the zero-cost path, and the one that keeps
working when the network (or a key) does not. If Ollama is not running the
error surfaces as a network error — start it with `ollama serve`.

## Per-provider key variables

Without `JOY_API_KEY`, the provider's own variable is read for
`JOY_PROVIDER`: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `DEEPSEEK_API_KEY`,
`GEMINI_API_KEY`, `MOONSHOT_API_KEY` (kimi), `ZHIPU_API_KEY` (glm),
`MINIMAX_API_KEY`, `XAI_API_KEY`, `OPENROUTER_API_KEY`,
`OPENCODE_ZEN_API_KEY`, `OPENCODE_GO_API_KEY`.

A missing key answers `PROVIDER_ERROR (-32000)` with where to get one and
which variable to set.

## Search and evaluation

| Variable | Purpose |
|---|---|
| `TAVILY_API_KEY` / `JOY_SEARCH_API_KEY` | upgrade `search_web` to Tavily (default: DuckDuckGo HTML) |
| `JOY_JUDGE_MODEL` | judge referee override |
| `JOY_GH_REPO` | gather's GitHub scan target |

## Dashboard and binaries

| Variable | Default | Purpose |
|---|---|---|
| `JOY_PORT` | `7777` | dashboard port; occupied ports scan upward |
| `JOY_DASHBOARD_DIR` | in-repo dist | location of the built frontend |
| `JOY_BIN` | — | explicit `joy` binary path for the Python SDK / TS clients |

## Chat gateways (joy-ts)

| Platform | Variables |
|---|---|
| Telegram | `TELEGRAM_BOT_TOKEN`, `TELEGRAM_ALLOW` (allowlist; unset = anyone) |
| Discord | `DISCORD_BOT_TOKEN`, `DISCORD_ALLOW` (enable Message Content Intent) |
| WeChat | `WECHAT_TOKEN`, `WECHAT_APP_ID`, `WECHAT_APP_SECRET`, `WECHAT_AES_KEY` (public 80/443 required) |
| Lark | `LARK_APP_ID`, `LARK_APP_SECRET`, `LARK_ALLOW` (open_id), `LARK_DOMAIN=lark` |

## MCP servers (`<home>/mcp.json`)

```json
{"servers": [
  {"name": "fs", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]},
  {"name": "notes", "url": "https://host/mcp", "auth_env": "NOTES_API_KEY"},
  {"name": "cloud", "url": "https://host/mcp", "oauth": true}
]}
```

`command` uses stdio and `url` uses Streamable HTTP. `auth_env` holds the
environment variable **name** (its value is sent as Bearer); `oauth: true`
uses browser authorization (`joy mcp login <name>`) with the token in
`mcp-auth/<name>.json` (0600). Unreachable servers are skipped with a warning;
Joy still starts.

## `settings.json` (config/write persistence)

`config/write` accumulates patches into `<home>/settings.json`, layered over
environment variables at boot. To clear a model override, submit an empty
string for the field. A patch failing validation is rejected wholesale —
invalid values never reach the file.
