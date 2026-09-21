# Operations

English | [简体中文](operations.zh.md)

Every way to run Joy day to day, the recommended environment, and
troubleshooting.

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

## MCP servers

Write `<home>/mcp.json` (stdio or HTTP servers); their tools merge into the
tool registry. For `"oauth": true` remote servers run `joy mcp login <name>`
first; expired tokens auto-refresh, and without a refresh_token rerun login.

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

## Data and backups

Only a few things under `<home>/` are worth backing up: `state.db` (memory
and conversation, the source of truth), `SOUL.md` (persona), `skills/`
(procedural memory), `mcp-auth/` (OAuth tokens, 0600). `traces/`,
`usage.jsonl`, `MEMORY.md`, and `outbox/` are regenerable derivatives.

## Troubleshooting

| Symptom | Action |
|---|---|
| `PROVIDER_ERROR (-32000)` missing key | fix the env-var chain, restart the process |
| 401 / 403 | key expired or region unreachable; change the key or `JOY_BASE_URL` |
| MCP tools disappeared | read the stderr warning: unreachable / not signed in (`joy mcp login`) / broken mcp.json |
| `[tools used: …]` in a reply | expected folding marker; tool activity is in history |
| "why did it answer that" | open `traces/<date>.jsonl` at that turnId |
| port 7777 taken | the dashboard scans upward; the startup log has the real port |
