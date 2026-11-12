# Operations

English | [简体中文](operations.zh.md)

Every way to run Joy day to day, the recommended environment, and
troubleshooting.

## Supported platforms

| Platform | Status | Why |
|---|---|---|
| macOS (Apple Silicon / Intel) | ✅ first-class | seatbelt sandbox, Keychain, `sh` hooks |
| Linux | ✅ first-class | bubblewrap sandbox, secret-service, `sh` hooks |
| Windows (native) | ❌ refuses to boot (exit 2) | no sandbox backend — per the "no sandbox, no execution" rule `run_command` could never run; hooks need `sh`; the credential store cannot read secrets back, so keys would sit in plaintext `secrets.json` |
| Windows + WSL2 | ✅ use the Linux build | everything above is a real Linux user space |

The refusal happens in the `joy` binary at startup (`cfg!(windows)`) with the
reasons printed — a half-working assistant that looks healthy is worse than a
clear no. CI keeps a Windows job for build + unit tests only (no sandbox, no
hooks tests); libraries compile everywhere, the binary simply will not boot.

## Terminal conversation

```bash
cd joy-rs && cargo build
JOY_PROVIDER=deepseek DEEPSEEK_API_KEY=… ./target/debug/joy
```

Streaming output; `/memory [term]` queries memory, `/sessions` lists
conversations, `/new` starts one, `/quit` exits. `Ctrl-C` in the terminal
exits the process — the dashboard uses `turn/interrupt` to cancel a turn.

## Web dashboard

```bash
cd joy-ts && npm run build --workspace @joy/dashboard   # build the frontend once
JOY_HOME=$HOME/.joy ./target/debug/joy dashboard        # → http://localhost:7777
```

The first screen shows config / sessions / memory; the chat area shows the
retrieval gate's ruling, tool calls, and streaming replies live. If port
7777 is taken it scans upward (10 tries).

- **The page remembers where you were.** Which conversation is open is kept in
  `localStorage` (`joy.session`, next to the working directory), so a reload lands back in
  that session instead of the oldest one; if that session was deleted meanwhile, the newest
  one opens instead — never an empty shell the list cannot show.
- **Switching back returns to where you were reading.** Each session keeps its own distance
  from the bottom (in memory, per page load): leave in the middle of a long conversation,
  come back, and you are at that same line — no dragging. A session opened for the first
  time in this sitting goes to the newest message, as does a reload; the position is placed
  instantly, not animated from the top.
- A cross-device ping (another device finished a turn) re-reads the history **without**
  moving the scroll position: if you are reading further up, you stay there.
- **The agent's own wording is English.** `SOUL.md`, the prompt sections, the tool-result
  messages and the planner prompt are English, whatever language you write in — the model
  answers in yours. `<home>/SOUL.md` is yours to edit: it is used verbatim, and only a file
  identical to the built-in seed is ever replaced. (A soul carried over from an older release
  counts as *yours*, so switching the prompt language means editing that file once.)
- **Two languages.** The button at the top right of the sidebar switches between Chinese and
  English and remembers the choice; a fresh browser follows `navigator.language` (Chinese for
  `zh-*`, English otherwise). Switching reloads the page — strings are baked into the DOM as
  they are painted. Model output, session titles and task text stay in whatever language they
  were written in: those are data, not labels.

### Putting it on a server (self-hosting)

```bash
export JOY_OPS_HOST=0.0.0.0            # a non-loopback bind requires a token, or startup is refused
export JOY_OPS_TOKEN=$(openssl rand -hex 16)
export JOY_HOME=/srv/joy/.joy
joy dashboard                          # → http://<server>:7777 (/api/* needs the Bearer token)
```

- The page asks for the token once on the first 401 and keeps it in
  localStorage; the static page itself needs no token (it must load before it
  can ask).
- **No key files to copy**: put `"key_source": "env:DEEPSEEK_API_KEY"` in
  `models.json` and export that variable in the server's environment. The key
  is only read into the memory of the process running the turn — never on
  disk, never in an `/api/*` response.
- TLS belongs to a reverse proxy (Caddy / nginx terminate HTTPS); Joy speaks
  plain HTTP.
- Product boundary: the server runs **orchestration**; connectors and MCP
  servers stay on the user's own device (local-first / no sandbox, no
  execution) — never put user-configured extensions on a shared server.

## MCP servers

Write `<home>/mcp.json` (stdio or HTTP servers); their tools merge into the
tool registry. For `"oauth": true` remote servers run `joy mcp login <name>`
first; expired tokens auto-refresh, and without a refresh_token rerun login.

The dashboard's capability center is the graphical door to the same file:
add / replace / delete a connector, with the transport and credential rules
enforced the same way (`url` **xor** `command`; `auth_env` is a variable
name, never a key). Changes take effect on the next start. Set
`JOY_MCP_CONFIG` for machine-wide servers — those rows show up greyed out
under a 「内置」 tag and every dashboard write refuses them.

Known operational limits (no log rotation, no daemonisation, missed schedule
runs are not backfilled, and more) are listed in
[limitations.md](limitations.md).

### Serving Joy's memory to other agents

The reverse direction also exists: `joy mcp serve` makes Joy itself an MCP
server over stdio, exposing five memory tools (`memory_search`,
`memory_remember`, `memory_forget`, `memory_list`, `memory_episodes`) so
other agents on this machine read and write the same facts. Only memory is
exposed — nothing that makes Joy act on someone else's behalf.

```json
{"mcpServers": {"joy-memory": {
  "command": "joy",
  "args": ["mcp", "serve"],
  "env": {"JOY_HOME": "/Users/you/.joy"}
}}}
```

Point any MCP client (Claude Code, codex, …) at that stanza and it shares
Joy's long-term memory. The server opens the same `state.db` through the same
`open()` (WAL + busy_timeout, so multiple processes coexist); facts written
by other agents are labelled `source: mcp` so the origin stays visible.

## Morning brief (cron-friendly)

```bash
JOY_GH_REPO=owner/repo joy gather
```

Four parallel scans → one synthesis → the summary prints and the draft lands
in `outbox/gather-<date>.md`. A failed source becomes an "unavailable" line;
the whole brief still ships.

## Python SDK embedding

```python
import os
from joyczl_agent import JoyClient

async with await JoyClient.connect() as client:   # spawns the joy app-server subprocess
    client.on_notification(lambda n: print(n.type))
    await client.request("turn/start", {"message": "hi", "stream": True})
```

Environment variables are decided by the embedding process (the subprocess
inherits them); `JOY_BIN` can point at the binary. Packaging:
`just build-python-bin` produces per-platform wheels.

## Packaging

```bash
docker build -t joy .
docker run --rm -p 7777:7777 -v joy-state:/home/joy/.joy \
  -e ANTHROPIC_API_KEY=… joy                 # dashboard
docker run --rm -it -v joy-state:/home/joy/.joy \
  -e ANTHROPIC_API_KEY=… joy app-server      # protocol on stdio
```

All state lives under `JOY_HOME` (`/home/joy/.joy` in the image), so one volume
carries it. The image installs `bubblewrap`, which is what the Linux sandbox
backend needs — without it `run_command` refuses to execute anything, by
design. A Homebrew formula template lives in `packaging/homebrew/joy.rb`
(source build via `cargo install`; fill in the release tarball's sha256).

## Scheduled jobs

`joy schedule` is a resident process that fires declarative jobs on a five-field
cron (`*`, `*/step`, `a-b`, `a,b`). Two ways to declare one:

* a skill whose frontmatter carries a schedule:

  ```markdown
  ---
  name: weekly-review
  description: summarize the week and draft the Monday brief
  schedule: 0 8 * * 1
  ---
  ```

* an entry in `<home>/schedules.json`:

  ```json
  {"jobs": [{"name": "standup", "cron": "0 9 * * 1-5", "prompt": "what's on today?"}]}
  ```

Each firing runs a full turn in its own session (`schedule:<name>`), so jobs
carry their own history and rolling summary; the answer lands in
`<home>/outbox/schedule-<name>-<stamp>.md` rather than interrupting chat. One
firing per minute at most, jobs run one at a time, skill edits are picked up
without a restart. Run it under launchd/systemd — or skip it entirely and call
`joy gather` from system cron if that is all you need.

Both sources show up in the dashboard's capability center (**抽屉 → 定时**), next to
Skills and MCP connectors: one row per job with its cron, its prompt, the next fire, the
last fire, and where the result goes. Rows that come from a skill are greyed and read-only
— that job *is* the skill, so the way to remove it is to edit `SKILL.md` or delete the
skill. Adding or deleting a `schedules.json` row writes exactly that file and nothing else:

```
name      morning-news        lowercase letters, digits and hyphens (the schedule key)
cron      0 8 * * 1-5         five fields: minute hour day month weekday
prompt    给我一份今天的科技简报   one sentence — it becomes one turn
delivery  outbox | notify | quiet
catch_up  错过之后补跑一次（默认）还是放弃
```

The panel is an editor for a file, not a scheduler. `joy schedule` still has to be running
(or something else has to call `joy gather`), and the panel says so at the bottom of the
list — a form that silently needed a resident process would be worse than no form.

The panel is not the only door: say it in chat — 「每天八点给我一份科技简报」— and the model
calls `schedule_job`, which writes the same file through the same validation. It answers
with the next fire time and the reminder that `joy schedule` has to be running. Deleting
from chat is `cancel_schedule`; listing is `list_schedules`.

Why a tool and not "let it write the file": `<home>/schedules.json` is outside every
workspace, so `write_file` refuses it and the UI asks you to pick a working directory — for
a file that has nothing to do with your project. Since 0.17 the declaration is Joy's own
state, written by a tool that owns it; nothing asks for a directory any more.

## Data and backups

Only a few things under `<home>/` are worth backing up: `state.db` (memory
and conversation, the source of truth), `SOUL.md` (persona), `skills/`
(procedural memory), `mcp-auth/` (OAuth tokens, 0600). `traces/`,
`usage.jsonl`, `MEMORY.md`, and `outbox/` are regenerable derivatives.

## Troubleshooting

| Symptom | Action |
|---|---|
| `PROVIDER_ERROR (-32000)` missing key | fix the env-var chain, restart the process |
| 401 / 403 | key expired or region unreachable; replace it with `/model add <name> <provider> --key new` |
| MCP tools disappeared | read the stderr warning: unreachable / not signed in (`joy mcp login`) / broken mcp.json |
| `[tools used: …]` in a reply | expected folding marker; tool activity is in history |
| "why did it answer that" | open `traces/<date>.jsonl` at that turnId |
| port 7777 taken | the dashboard scans upward; the startup log has the real port |

## Compacting and looking at memory

* `/compact [focus]` in the REPL — or **现在压缩** in the dashboard's memory center — folds
  the older turns into the rolling summary **now**, keeping the newest two in the prompt. The
  focus is one extra sentence in the summarizer's material ("keep the migration plan in
  mind"), not a different prompt: summaries have to stay comparable turn to turn.
* The memory center (drawer → 记忆) has three read-only views. **账本**: one row per turn,
  open it for that turn's entry stream; `折叠` marks a backfilled turn whose tool output only
  exists as a summary. **记忆库**: catalog on top, one page below, and the `ledger:…` chips
  walk back to the rows the page was distilled from. **上下文**: the six-section band
  (soul / memory / skills / tools / history / summary) with used, budget and a notice at
  `JOY_CONTEXT_WARN_RATIO`.
* The band measures what **grows with the session**. Four things ride in every prompt
  uncounted: the clock, the todo board, the capability note (the two or three lines that
  appear when `JOY_EXEC` / `JOY_WRITE` are off), and the project-conventions section read
  from `AGENTS.md` / `AGENTS.override.md` / `CLAUDE.md` between your working directory and
  the enclosing `.git`. The first three are a few dozen tokens; the conventions are capped
  at 32 KB — so in a repository with a fat `AGENTS.md` the band *understates* the prompt by
  that file. That is the trade: the band answers "what grows within a session", and a file
  that is constant per directory does not grow.
* A turn may spend at most `JOY_MAX_ITERATIONS` model calls (default 32). The **last one is a
  wrap-up**: the tools are taken away and the model has to answer in three sentences — done /
  stuck / what it needs next. A long turn therefore ends with something you can act on
  instead of an apology. Oversized tool results are stubbed oldest-first before each request
  (`JOY_TOOL_RESULT_TOTAL_CHARS`), so one chatty command cannot fill the window by itself.
* `joy memory rebuild --page <slug>` regenerates one page from its source turns. It is a CLI
  command on purpose: a whole-page regeneration is a model call, and a model call should be
  something you asked for, not something a button did quietly.

## Crash recovery has an age limit

A checkpoint left `open` means the last process died mid-turn. At startup Joy resumes it —
the human asked for that work, and continuing is not the model assigning itself anything.
Since 0.17 that is true **within a window**: `JOY_RESUME_MAX_AGE` (minutes, default `60`)
bounds how old the row may be. `0` turns the limit off (resume everything, the old rule).

Why an hour and not "forever": a checkpoint is crash recovery, so its real span is seconds
to minutes. Resuming a two-day-old turn is not the same turn continued — "give me today's
tech news", answered two days later, answers a question nobody is asking, and it spends a
real model call plus a real fetch to do it. The stale row is marked `abandoned` and the
startup log says so:

```
(joy) 对账：回合 t… 停在 2 天前（上限 1 小时），没有自动续跑 —— 要接着做，把那句话再说一遍
```

The ledger keeps the row either way: `abandoned` is a status, not a deletion.
