# Security

English | [简体中文](SECURITY.zh.md)

Joy runs on the user's own machine, with the user's own API keys, touching the
user's own data. The following is the complete security model — every item is
a structural guarantee, not a convention.

## Credential handling

* API keys exist only in **environment variables**: never in config files,
  protocol payloads, state.db, or logs. A missing key error names where to get
  one and which variable to set.
* MCP OAuth tokens land in `<home>/mcp-auth/<server>.json`: written to a temp
  file, chmod 0600, then atomically renamed — the secret is never briefly
  world-readable and an interrupted write cannot corrupt the good file.
* `mcp.json` stores the **name** of an environment variable in `auth_env`,
  never the value — config files eventually get pasted into issues.

## Tool capability boundaries

* `send_message` **only writes an outbox draft**; the code path has no send
  capability at all.
* The gather graph is **structurally tool-free**: the only model call carries
  no tools parameter, so the model holds no schema it could use to send, merge,
  or create anything. "Propose, never act" is structural.
* `create_event` idempotence is guaranteed by a (title, start) unique index —
  a confused model cannot triple-book a meeting.
* External reads (gather's GitHub scan) use the user's own `gh` CLI
  credentials, independent of the model-visible tool switches.

## Running commands (JOY_EXEC)

`run_command` is the highest-privilege thing in the project, so it is
**off by default** and gated three times:

1. **Hard deny list** (`joyczl-tools/src/exec.rs`) — catastrophic commands
   (`sudo`, `mkfs`, `dd`, fork bombs, download-piped-into-shell) never run,
   no matter what is configured. This gate is not configurable; a fuse you
   can switch off is not a fuse.
2. **Allowlist** — `JOY_EXEC_ALLOW` must match, or nothing runs. An empty
   list denies everything; default-deny, never default-allow.
3. **Sandbox** — commands run under macOS `sandbox-exec` / Linux
   `bubblewrap` with write access confined to the working directory, the
   Joy home and temp. **If no sandbox is available, the command is
   refused** — Joy never runs a command unsandboxed "to make it work".

The sandbox also cuts the network: sandboxed commands run offline unless
`JOY_EXEC_NETWORK=1` is set, and extra writable roots can be opened with
`JOY_EXEC_WRITABLE_ROOTS` (each must be an existing absolute directory).

With `JOY_APPROVAL=on-request` the allowlist gate becomes a question instead
of a wall: an unmatched command emits `approvalRequested` and waits (120s by
default). **No answer means no run** — timeouts, a closed connection, a late
reply and "no one is listening" all resolve to a refusal. The hard deny list
and the sandbox cannot be approved away; only the allowlist gate is
negotiable.

Known limits, stated so they do not quietly grow: the hard deny list is
substring matching, so cleverly obfuscated commands can slip past it (the
allowlist and the sandbox are the real defences); "remember this command"
writes the exact command and needs a restart; policy is read once at startup.
The full list, with the file to open for each, is
[docs/limitations.md](docs/limitations.md).

## Inputs and paths

* Skill names are forced to lowercase slugs (create_skill) and `install`
  validates the frontmatter — path traversal is rejected before any write.
* FTS5 query strings are built by `to_match_expr`: each term quoted, so `*`,
  `"`, and `:` in model input never become search syntax.
* MCP tool names are mapped to `^[a-zA-Z0-9_-]{1,64}$`, and the original name
  is always sent back to the server — renaming only affects what the model
  calls it, never dispatch.

## OAuth

* The authorization-code flow uses PKCE (S256) and state validation; a forged
  callback is refused before the token exchange.
* Dynamically registered client_info and tokens are stored per server, 0600.
* At startup a missing token only warns and skips the MCP server — **the app
  never opens a browser on its own**; sign-in happens only when the user
  explicitly runs `joy mcp login`.

## Gateways

* Telegram / Discord / Lark: without the `*_ALLOW` allowlist anyone can talk —
  startup behavior and docs both say so explicitly.
* WeChat: signature verification + `MsgId` deduplication (the three retries
  cannot produce duplicate replies).
* Each gateway derives session ids from platform identity; conversations
  never leak across gateways or users.

## What fail-open means for security

The retrieval gate and the triage graph "fail open" only for **answer
quality** (more memory consulted / more time spent) — they never widen
capability. Any failure falls back to the plain loop, never to more
permissions.
