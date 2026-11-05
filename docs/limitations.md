# Known limits and where to pick them up

English | [简体中文](limitations.zh.md)

Everything here is a **known** boundary — either a deliberate design decision
or work that has not been done yet. Nothing on this list is a surprise bug.
Each entry says what the boundary is, why it exists, and which file to open
first if you want to change it.

Reading convention: **Deliberate** means "this is the intended behaviour; a
change here is a design change and belongs in a discussion first". **Gap**
means "we would like this, nobody has built it".

## Multiple models and where keys live

- **One configuration path only**: `JOY_API_KEY` / `JOY_BASE_URL` / `JOY_MODEL`
  / `JOY_SMALL_MODEL` are no longer read and `config/write` accepts no model
  fields — models change only through `model/*` (REPL `/model`, dashboard panel).
  `JOY_PROVIDER` now means just "the fallback provider when nothing is
  configured". If you relied on environment variables, you are now unconfigured —
  run `/model add` once as the message says.
- **No usable keychain on Windows**: its credential manager stores entries but
  cannot read secrets back, so on Windows (and anywhere `security` /
  `secret-tool` is missing) keys fall back to `<home>/secrets.json` —
  **plaintext**, mode 0600 on Unix, which Windows has no notion of. `model/save`
  reports `file` honestly and the UI repeats it verbatim.
- **Keychain commands time out after 5s**: `security` can prompt or hang in an
  unattended session; a timeout counts as failure and falls back to the file —
  one credential operation must never freeze the server.
- **Switching affects the live process only**: it rewrites in-process Settings and
  re-resolves the provider; the configuration itself lives in
  `<home>/models.json`, so the next start still uses your last pick.
- **The model cannot switch models**: `model/*` is human-only — the model has no
  such tool and never sees a key.

## Notifications and audit

- **Notifications are best-effort**: a failed send (network down, endpoint 5xx)
  is one stderr line — **no retries, no delivery ledger**. A five-state
  delivery ledger (pending/attempting/delivered/…) waits for tier B (when job
  results can wake a session); today a notification is a shout that may not
  carry. The Slack `{"text": …}` format also works with self-hosted ntfy/Gotify
  endpoints; DingTalk/WeCom signed formats are not implemented.

- **Hub accounts are real but bare**: register/login issues per-device
  tokens (登录即配对), yet there is **no password recovery flow** (the owner
  edits users.json via CLI), **no rate limiting / lockout** on login (one
  short delay per miss, nothing more), **no email verification or 2FA**, and
  **no invite links or OAuth** — admission is a seat cap plus the first-user
  loopback rule. Per-user quotas likewise record but do not enforce (usage
  data is accumulating; enforcement waits for billing).
- **Cross-device sync is a knock, not a feed**: `/api/events` pings say
  "something happened" and the client re-pulls everything it shows. There is
  no message-level delta protocol, no offline queue, no push notifications
  (APNs/FCM) — a phone learns about finished work when the PWA is open.

## Long-running and the daemon lock

- **The app-server takes a daemon lock**: only one app-server per `<home>` at a
  time (`<home>/app-server.lock`, holding the PID; a lock left by a dead process is
  reaped by the next start). Two resident processes each holding their own
  in-memory state (approvals, goals, jobs) while writing one database is an
  incident, not a feature — so it is a hard error, not a warning. The REPL and
  `joy schedule` take no lock: they are separate, one-shot processes.
- **Missed schedule firings are caught up**: `joy schedule` persists its
  last-fired table to `<home>/schedules-state.json`; at boot each job looks back
  over the minutes since its last firing (capped at seven days) and fires the
  latest missed slot **once**, marked as a catch-up in the prompt. Three days of
  downtime does not produce three days of catch-ups.

## Execution (`run_command`, `JOY_EXEC`)

`read_file` / `list_dir` are on by default and bypass the execution gate — their
trust boundary is the **process user's OS permissions**: the model can read any
file that user can read (as broad as typical read-only tools). Put files you do not
want the model to see where that user cannot read them. `fetch_url` mirrors
this: its network access does **not** go through the `run_command` sandbox (it is
a direct in-process request) — the guard is the SSRF address check plus
external-content defusal, not a sandbox. The hand-rolled HTML converter is
weaker than readability-class libraries on heavy layouts; for high-quality
extraction use `run_command` + curl or an MCP fetching service.

* **Deliberate** — Sandboxed commands run **offline** by default
  (`JOY_EXEC_NETWORK=1` to allow network); writes are confined to the working
  directory, the Joy home and temp, plus anything listed in
  `JOY_EXEC_WRITABLE_ROOTS`. Commands that need to download something need
  both switches.
  → `joy-rs/joyczl-tools/src/exec.rs` (`sandbox_argv`)
* **Deliberate** — The write tools (`write_file` / `edit`, `JOY_WRITE=1`) have
  **no process sandbox**: the write happens in-process, so the hard gate is the
  writable roots (workdir + home + `JOY_EXEC_WRITABLE_ROOTS`, resolved through
  symlinks). Outside them is refused before approval is consulted — approval
  only unlocks writes already inside the boundary. `require_str` trims, so the
  write tools take content via a no-trim reader: a trailing newline is data,
  not noise.
  → `joy-rs/joyczl-tools/src/fs_write.rs` (`contained`, `raw_str`)
* **Deliberate** — Spilled command output is cleaned by **age only**
  (`spill/` keeps 7 days, pruned at startup), not by a size quota. A quota that
  guesses wrong either fills the disk or deletes something you wanted; a
  time-based sweep is predictable. Nothing else in `JOY_HOME` is rotated.
  → `joy-rs/joyczl-tools/src/exec.rs` (`prune_spill`)

* **Deliberate** — The hard deny list is substring matching, so obfuscated
  variants can slip past it. It is a seatbelt, not a proof: the allowlist and
  the sandbox are what actually hold.
  → `joy-rs/joyczl-tools/src/exec.rs` (`HARD_DENY`)
* **Deliberate** — Approval is per *mode*, not per tool class. `JOY_APPROVAL`
  has `never` (default) and `on-request`; the finer-grained version (ask for
  exec but not for MCP, say) is not there because only exec has a gate to ask
  at — the config would be describing something that does not exist.
  → `joy-rs/joyczl-config/src/lib.rs` (`approval`)
* **Deliberate** — "Remember this command" writes the command **exactly as
  approved** (no wildcard) into `settings.json`, and takes effect on the next
  start: the exec policy is read once at startup. Saying "remembered" and
  silently widening the rule would be worse.
  → `joy-rs/joyczl-app-server/src/lib.rs` (`remember_command`)
* **Deliberate** — MCP reverse requests (elicitation/sampling) get an explicit
  "not supported" error instead of complete support. Answering them properly
  needs a UI contract for arbitrary server-authored questions.
  → `joy-rs/joyczl-mcp/src/transport.rs`
* **Deliberate** — Without a working sandbox, *nothing* runs. On Linux this
  means `bwrap` must both exist *and* be able to create a namespace: Ubuntu
  24.04 restricts unprivileged user namespaces, so availability is probed by
  actually running `bwrap` once per process.
  → `joy-rs/joyczl-tools/src/exec.rs` (`sandbox_backend`, `bubblewrap_runs`)
* **Gap** — Policy (`JOY_EXEC`, `JOY_EXEC_ALLOW`, `JOY_WRITE`) is read once at
  startup; `config/write` does not rebuild the tool registry. Restart the process.
  → `joy-rs/joyczl-app-server/src/lib.rs` (`builtin_tools`)
* **Gap** — The hard-deny list is substring matching with a hand-written
  pipe-into-shell check. Clever obfuscation (`bash -c "$(printf …)"`) will not
  be caught by it. The allowlist and the sandbox are the real defences.
  → `joy-rs/joyczl-tools/src/exec.rs` (`HARD_DENY`, `pipes_into_a_shell`)

* **Deliberate** — Model calls are retried only for 429/5xx/network, at most
  `JOY_LLM_RETRIES` times (default 2), inside a 30 second budget, and never by
  switching provider. A provider that is down stays down until you change
  something.
  → `joy-rs/joyczl-provider/src/retry.rs`

* **Deliberate** — The report hardening is a **speed bump, not a sandbox**: lines
  imitating a role prefix (`Human:` / `System:`) or our own control markers
  (`<system-reminder>`, `[tools used:`) get a backslash, and the report carries a
  "this is a transcription" header. It does **not** judge maliciousness (that is
  neither possible nor reassuring) and it does **not** change any tool call's
  permission check — permissions are the gates' business. The real defences are the
  sandbox, the allowlist and approval.
  → `joy-rs/joyczl-tools/src/report.rs`
* **Deliberate** — A subagent has no approval path: where the parent would
  ask you, the child simply is refused (`JOY_APPROVAL` never reaches it). A
  subagent that blocks on a human is worse than a subagent that cannot do the
  thing.
* **Deliberate** — A subagent's conversation is not persisted (no
  `subagent:*` entries in `session/list`) and its internal events are not
  streamed; the parent sees one tool call and its conclusion. What it used
  (including failed calls) is appended to that conclusion.
  → `joy-rs/joyczl-app-server/src/subagent.rs`

* **Deliberate** — Memory de-duplication keys on `(subject, normalised content)`
  (case and surrounding whitespace do not count) and **not** on `kind`: if the same
  sentence was first stored as `fact` and later arrives as `feedback`, the **first**
  classification stays. It does not deduplicate across languages.
  → `joy-rs/joyczl-state/migrations/0008_memory_dedup.sql`
* **Deliberate** — Memory kinds are a closed set of five (`fact` is the
  fallback); a model inventing its own category lands in `fact` rather than
  creating a taxonomy. Nothing groups by kind in retrieval yet — it is stored
  and returned, not used to filter.
  → `joy-rs/joyczl-state/src/facts.rs` (`KINDS`)

* **Deliberate** — The sandbox is Unix-only (macOS `sandbox-exec`, Linux
  `bubblewrap`), and the boundary is enforced up front: on native Windows the
  `joy` binary **refuses to boot** (exit 2) with the reasons printed — a
  half-working assistant that looks healthy is worse than a clear no. WSL2 is
  the supported way to run Joy on Windows (see the matrix in
  docs/operations.md). CI keeps a Windows job for build + unit tests only,
  named so the boundary is visible.
  → `.github/workflows/ci.yml`, `joy-rs/joyczl-cli/src/main.rs` (`windows_blocker`),
  `joy-rs/joyczl-tools/src/exec.rs`
* **Deliberate** — Context compaction does not preserve file-operation state
  (which files were read or written earlier in the session, the way pi's
  harness does). What survives eviction is the rolling summary plus the
  `[tools used: …]` fold; a model that needs the exact earlier diff has to read
  the file again.
  → `joy-rs/joyczl-memory/src/compaction.rs`, `joy-rs/joyczl-app-server/src/turn.rs`
* **Deliberate** — Token counts are estimated with a tiktoken encoding, applied
  across providers that do not use it. The estimate decides *when* to compact,
  never what is billed: the provider's reported `usage` is the authoritative
  number. Being wrong here moves compaction earlier or later, it does not
  corrupt anything.
  → `joy-rs/joyczl-provider/src/tokens.rs`

* **Deliberate** — The MCP circuit breaker is **in-process** and fixed at "3
  consecutive failures → 60 seconds": no persistence, no knob, and after a server
  restarts Joy will still try once. It exists to stop a dead server costing one
  timeout per turn, not to be an availability metric.
  → `joy-rs/joyczl-mcp/src/lib.rs` (`Breaker`)
* **Deliberate** — The per-turn tool-result budget only touches results that are
  individually large. A hundred medium results overflowing the cap are left alone (one
  stderr line, no content change): a file per result would cost more than the context it
  saves.
  → `joy-rs/joyczl-loop/src/budget.rs`
* **Deliberate** — Calibration is a **ratio** (observed / estimated) clamped to 0.5–2.0,
  so one odd request cannot drag the budget off. Anything sharper means a real tokenizer
  per provider family.
  → `joy-rs/joyczl-state/src/chat.rs` (`context_factor`)

* **Deliberate** — Hooks have **two handler kinds**: shell (`command`) and HTTP
  (`url` — same payload POSTed, same decision JSON back, reqwest already in the
  tree). The verb is fixed at POST: the payload *is* the message; a `method`
  field nobody sets would be invention. Still not done: Claude Code's
  mcp_tool / prompt / agent variants — each needs its own contract, and shell
  already expresses them (`joy mcp`, a model CLI). Want an LLM to judge? Call one.
* **Deliberate** — Hooks are **Unix-first**: commands run through `sh`, so Windows
  needs a real executable. The CI Windows job skips these tests rather than pretending
  they pass. Making Windows first-class means adding a `cmd /C` branch and hook tests
  that run there.
* **Deliberate** — A `hooks.json` changed while running is **not executed** (with the
  reason printed); a restart picks up the new content. That trades a little
  convenience for "nothing can swap your gate silently".
* **Deliberate** — A blocking `Stop` re-runs the turn **once**. Real automatic
  continuation is the goal loop's job, with its round cap, judge and human-authority
  boundary.

* **Deliberate** — The todo list is **in-process**: one per session, gone on
  restart. It is working memory for this run; anything that must survive goes to
  `save_note` or `schedule`. It is also capped (20 items, 4000 characters each,
  one `in_progress` at a time) — a list is working memory, not a log. Subagents do
  not have it: the list belongs to the **parent** task line.
  → `joy-rs/joyczl-tools/src/todo.rs`

* **Partly deliberate** — The background job output ring is still in-process
  64 KiB (overflow drops the oldest bytes and says how many), but "they die with the
  process" is half-fixed: start/finish are written to the jobs ledger in state.db
  (migration 0009) and output streams to `<home>/jobs/<pid>-<id>.log` (1 MiB cap,
  stated when hit). After a crash, boot reconciliation marks still-running jobs
  `interrupted` and keeps their output readable — **never auto-rerun** (the
  consistent choice across the reference harnesses: auto-rerun is an incident
  amplifier; the rerun decision belongs to the human or model reading the ledger).
  Finished results still land in `<home>/outbox/jobs/`. For "keep running across a
  restart", make it a `joy schedule` entry — that path catches up missed firings.
* **Deliberate** — Background commands go through the **same gate** as foreground
  ones (sandbox, network, allowlist, approval): the background is not a side door
  around approval. There is no PTY either — interactive programs (`vi`, `top`) do
  not belong in the background to begin with.

* **Deliberate** — The goal loop's judge **sees only the conversation**: a
  cheap model, no tools, no retrieval. Its question is "does this conversation show
  the goal met", and giving it more would just have it start doing the work. It
  follows that its verdict can be wrong — which is why it may only *continue a
  round*, never let anything past a permission check.
* **Deliberate** — Goals are set, changed and cleared by humans only (`/goal` or
  `goal/set`): the model has no path to them. After a terminal outcome (satisfied /
  impossible / round-limit / judge failure) the loop stops by itself and waits for a
  human.

## Concurrency

* **Deliberate** — Two concurrent turns on the **same session are not serialised**
  (`turn/start` is handled concurrently so `turn/interrupt` can get in). The
  consequences are contained by three guards, none of which can corrupt data:
  consolidation is an **atomic claim** (the loser never calls the model), the
  rolling summary has a **monotonic guard** (only a summary covering more turns
  is accepted), and memory writes have a **de-duplication gate**. What is left is
  "waits a bit longer".
  → `joy-rs/joyczl-memory/src/consolidation.rs`, `joyczl-state/src/chat.rs`
* **Deliberate** — SQLite waits **10 seconds** for a lock, set on the connect
  options so every connection carries it. Multiple processes writing at once
  (REPL, dashboard, `joy mcp serve`, `joy schedule`) queue instead of failing;
  if 10s is not enough the answer is fewer concurrent writers, not a longer
  timeout.
  → `joy-rs/joyczl-state/src/db.rs`
* **Deliberate** — There **is** a per-session *write-section* lock, held only
  during a turn's write sections: compaction (`refresh` inside `load_history`)
  and the tail section (history append / context calibration / consolidation /
  MEMORY.md mirror). **Model-call sections never hold it** — that would stall
  the session and keep interrupt out; `turn/interrupt` writes no session state,
  so it is orthogonal to the lock. Locks are created lazily per session and
  **never removed** (removal would let two concurrent turns hold two different
  locks). The three SQLite-level guards stay: they prevent *double-write
  corruption*; the lock prevents *read/write interleaving* — complementary.
  → `joyczl-app-server/src/lib.rs` (`session_write_lock`), `turn.rs` (two locked sections)
* **Not done** — `prune_spill` at startup has a theoretical race with another
  process writing (it only deletes files older than seven days).

## Graph workflows

* **Deliberate** — Workflow topology is a **JSON DAG**
  (`joy-rs/joyczl-graph/src/workflows/definitions/*.json`), but it is **compiled
  in** via `include_str!`: there is no `<home>/workflows/` runtime load or
  override. Reason: a workflow *is* behaviour (the route whitelist and
  `max_visits` guards are its safety properties); loading behaviour from disk
  means changing control flow without review. To change the topology, edit the
  JSON and recompile. Load-time validation (acyclic static edges, routers must
  be registered built-ins, every node must have a binding) lives in
  `workflow_def.rs`.

## Model routing

Three tiers only (`fast`/`chat`/`reasoning`) — no cost or speed labels on
profiles (the usage ledger already bills per model), no vision tier routing
(images ride on the turn's tier, gated by the provider's `vision` flag — see
"Files & uploads"), no sibling-tier failover chain: if a routed profile fails
to resolve, the turn falls back to the active profile rather than trying a
sibling tier. The L1 classifier sees only the message text (first 4k chars) —
not tools, not history, not images.

* **Deliberate** — The model catalog (`<home>/models-catalog.json`, see
  configuration.md) is read **once at boot**: switching models re-resolves the
  provider, it does not re-read the catalog; a new entry needs a restart.
  Broken rows cost one stderr line each and are skipped; a broken file falls
  back to the built-in table — never a failed boot.
  → `joy-rs/joyczl-provider/src/catalog.rs` (`load`)
* **Deliberate** — The embedding endpoint (hybrid retrieval) still selects
  from the **built-in** provider table only: a catalog-only provider gets no
  embeddings, and hybrid retrieval degrades to keyword-only (its normal
  off/degraded mode).
  → `joy-rs/joyczl-provider/src/embed.rs` (`lookup`)

## Files & uploads

Dashboard uploads are **two routes**: non-image files are references — a file
lands in `<home>/uploads/<date>/` under a server-generated name (only the
extension is taken from the user, whitelist-checked), and its absolute path is
injected into the message; the model reads it with `read_file` if it decides to.
**Images are the exception**: they ride as real `ContentBlock::Image` blocks —
the server reads the bytes, base64s them, and gates on the provider's `vision`
capability up front (see configuration.md for the per-provider flag). Everything
about the attachment is checked before the turn starts: containment inside
`<home>`, existence, media type by extension, 20 MiB per image.

* **Gap** — Attachments are **not replayed on session restore**: `chat_log`
  stores the message text (which mentions the upload path), and history
  rebuilds as text — a restored session describes the image instead of
  re-showing it. Persisting the bytes (or a reference resolution pass on
  restore) would close this.
  → `joy-rs/joyczl-app-server/src/turn.rs` (`resolve_attachments`, `load_history`)
* **Deliberate** — The `vision` flag describes the provider's **default
  models** and errs toward `false`: a wrong `false` costs one capability; a
  wrong `true` costs a round trip and a cryptic remote 400. Overridable per
  provider in `models-catalog.json`.
  → `joy-rs/joyczl-provider/src/lib.rs` (`ProviderInfo::vision`)

## Memory

* **Deliberate** — Exact duplicates never enter the table: migration 0008 puts a
  unique index on `(subject, lower(trim(content)))` and `add` resolves the conflict
  by returning the existing row with `is_new = false` — a rerun of consolidation or
  a repeated `save_note` is noise, not an error. **Not done** is merging
  near-duplicates: the same fact reworded lands as a second row. A
  similarity-based merge pass would go here.
  → `joy-rs/joyczl-state/src/facts.rs` (`add`), `joy-rs/joyczl-state/migrations/0008_memory_dedup.sql`
* **Gap** — `MEMORY.md` is a *generated view*, rewritten after every turn from
  `facts` + `episodes`. Editing it does nothing; state.db is the source.
  Making it editable would need a write-back path and conflict rules.
  → `joy-rs/joyczl-memory/src/lib.rs` (`export_markdown`)
* **Deliberate** — Temporary-statement filtering is a marker list (both
  languages), not a classifier. It will miss unusual phrasing.
  → `joy-rs/joyczl-memory/src/consolidation.rs` (`TEMPORARY_MARKERS`)
* **Gap** — Skill triggering tokenises ASCII alphanumerics only, so a skill's
  trigger words must be written in English even when the conversation is in
  Chinese. CJK tokenisation (or a per-skill `triggers:` list) is the fix.
  → `joy-rs/joyczl-memory/src/skills.rs` (`tokens`)

## Retrieval

* **Deliberate** — Hybrid retrieval needs an embedding endpoint
  (`JOY_EMBEDDINGS=1` + `JOY_EMBED_MODEL`). Off by default: keyword search
  works with no model and no network.
  → `joy-rs/joyczl-provider/src/embed.rs`
* **Gap** — Vectors live in a `facts.embedding` JSON column compared with a
  full scan in Rust. Fine at personal scale (thousands of rows), wrong at
  millions. The column is the migration starting point.
  → `joy-rs/joyczl-state/migrations/0005_embeddings.sql`
* **Gap** — Facts written before the switch was on have no vector and only
  appear via the keyword leg until `joy memory reindex` runs. Episodes are not
  vectorised at all (they carry dates, which does most of the work).
  → `joy-rs/joyczl-memory/src/retrieval.rs`
* **Gap** — `MIN_SIMILARITY = 0.30` is a constant calibrated by feel, not per
  model. Different embedding models have different similarity scales.
  → `joy-rs/joyczl-memory/src/retrieval.rs`

## Context

* **Deliberate** — Rolling summaries live in their own `context_rollups` table;
  `chat_log` keeps the original turns untouched. "Re-read history" therefore
  shows the real dialogue, not the summary. Do not "fix" this without deciding
  what the archive is for.
  → `joy-rs/joyczl-state/src/chat.rs`, `joy-rs/joyczl-memory/src/compaction.rs`
* **Deliberate** — A dead summariser degrades to a deterministic excerpt (and
  says so in the prompt) rather than dropping the evicted turns.
  → `joy-rs/joyczl-memory/src/compaction.rs` (`fallback_summary`)
* **Gap** — Summaries are never re-summarised: a very long session keeps one
  growing paragraph. Compaction of the compaction (hierarchical rollups) is
  not implemented.
  → `joy-rs/joyczl-memory/src/compaction.rs` (`roll_forward`)

## Turns and protocols

* **Deliberate** — `turn/interrupt` cancels the loop's model call and running
  tools; it does *not* cancel the retrieval gate or the compaction call that
  happen before the loop. An interrupt during those takes effect when the loop
  starts. Racing them would need the token threaded through those calls.
  → `joy-rs/joyczl-app-server/src/turn.rs`
* **Gap** — `joy mcp serve` implements `initialize`, `tools/list` and
  `tools/call` only. MCP `resources`, `prompts`, sampling and notifications are
  not implemented.
  → `joy-rs/joyczl-mcp/src/server.rs`
* **Gap** — MCP OAuth requires a manual `joy mcp login <name>`; there is no
  background re-authorisation when a refresh token finally expires.
  → `joy-rs/joyczl-mcp/src/oauth.rs`, `joy-rs/joyczl-cli/src/mcp_cmd.rs`
* **Deliberate** — Several processes can open `state.db` (app-server, the
  REPL, `joy mcp serve`, `joy schedule`) — that is what WAL plus
  `busy_timeout` are for — but nothing coordinates *turns* across them. Two
  turns running in different processes interleave freely.
  → `joy-rs/joyczl-state/src/db.rs`

## Gateways

* **Gap** — Discord reconnects with a fresh IDENTIFY instead of RESUME, so
  messages sent during the gap are lost. Needs `session_id` +
  `resume_gateway_url` bookkeeping.
  → `joy-ts/packages/gateway/src/discord.ts`
* **Gap** — The WeChat, Lark and Discord gateways have never been pointed at
  the real platforms. They are tested against fakes only.
  → `joy-ts/packages/gateway/`
* **Deliberate** — WhatsApp and the voice gateway are not implemented. Voice
  was in the original Python project and was not ported; nothing in this repo
  claims otherwise.

## Operations

* **Gap** — No log rotation. Traces are one file per day
  (`traces/YYYY-MM-DD.jsonl`) but `usage.jsonl` grows forever.
  → `joy-rs/joyczl-app-server/src/trace.rs`
* **Gap** — `joy schedule` and `joy dashboard` are plain foreground processes;
  supervising them (launchd / systemd / Docker restart policy) is the
  operator's job. No daemonisation, no PID files.
  → `joy-rs/joyczl-cli/src/schedule.rs`, this repo's `Dockerfile`
* **Deliberate** — Scheduled jobs fire at minute granularity, one at a time,
  and are skipped if the process was not running at that minute. Missed runs
  are not backfilled.
  → `joy-rs/joyczl-cli/src/schedule.rs` (`due_now`, `run`)
* **Gap** — The Homebrew formula is complete for `v0.5.0` (real `sha256`, checked
  against the tag archive) but it lives in the repo, not in a tap: today
  `brew install ./packaging/homebrew/joy.rb` needs a checkout, and publishing a
  tap is a separate decision. The hash must be refreshed on every release.
  → `packaging/homebrew/joy.rb`

## Quality system

* **Deliberate** — Deterministic evals run against a scripted provider: no
  keys, no network, same answer every time. Judge evals need a real model and
  a real key, so they are a separate command (`joy judge`).
  → `joy-rs/joyczl-eval/src/`, `evals/`
* **Deliberate** — Scenarios that need something the machine may not have
  (python3, a shell, a working sandbox) declare `prereq` and are skipped —
  counted as skipped, never as passed.
  → `evals/deterministic/exec.jsonl`, `evals/deterministic/hooks.jsonl`
* **Gap** — The embedding path has no end-to-end test against a real embedding
  server: fusion and degradation are unit-tested, the HTTP call itself is not.
  → `joy-rs/joyczl-memory/src/retrieval_tests.rs`

## Configuration

* **Deliberate** — `config/write` persists to `<home>/settings.json`, which is
  applied *on top of* the environment. A saved patch therefore outranks
  `JOY_*` for that field until it is cleared (empty string clears model
  overrides; an empty patch removes the file).
  → `joy-rs/joyczl-config/src/lib.rs` (`apply_patch`, `save_patch`)
