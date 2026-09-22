# Known limits and where to pick them up

English | [简体中文](limitations.zh.md)

Everything here is a **known** boundary — either a deliberate design decision
or work that has not been done yet. Nothing on this list is a surprise bug.
Each entry says what the boundary is, why it exists, and which file to open
first if you want to change it.

Reading convention: **Deliberate** means "this is the intended behaviour; a
change here is a design change and belongs in a discussion first". **Gap**
means "we would like this, nobody has built it".

## Execution (`run_command`, `JOY_EXEC`)

* **Deliberate** — Sandboxed commands run **offline** by default
  (`JOY_EXEC_NETWORK=1` to allow network); writes are confined to the working
  directory, the Joy home and temp, plus anything listed in
  `JOY_EXEC_WRITABLE_ROOTS`. Commands that need to download something need
  both switches.
  → `joy-rs/joyczl-tools/src/exec.rs` (`sandbox_argv`)
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
* **Gap** — Policy (`JOY_EXEC`, `JOY_EXEC_ALLOW`) is read once at startup;
  `config/write` does not rebuild the tool registry. Restart the process.
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

* **Deliberate** — A subagent has no approval path: where the parent would
  ask you, the child simply is refused (`JOY_APPROVAL` never reaches it). A
  subagent that blocks on a human is worse than a subagent that cannot do the
  thing.
* **Deliberate** — A subagent's conversation is not persisted (no
  `subagent:*` entries in `session/list`) and its internal events are not
  streamed; the parent sees one tool call and its conclusion. What it used
  (including failed calls) is appended to that conclusion.
  → `joy-rs/joyczl-app-server/src/subagent.rs`

* **Deliberate** — Memory kinds are a closed set of five (`fact` is the
  fallback); a model inventing its own category lands in `fact` rather than
  creating a taxonomy. Nothing groups by kind in retrieval yet — it is stored
  and returned, not used to filter.
  → `joy-rs/joyczl-state/src/facts.rs` (`KINDS`)

* **Deliberate** — The sandbox is Unix-only (macOS `sandbox-exec`, Linux
  `bubblewrap`). On Windows there is no backend, so `run_command` refuses to
  run anything — which is the intended reading of "no sandbox, no execution",
  not a bug. CI has a Windows job that runs everything *except* the sandbox
  tests, named so the boundary is visible.
  → `.github/workflows/ci.yml`, `joy-rs/joyczl-tools/src/exec.rs`
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

* **Deliberate** — The per-turn tool-result budget only touches results that are
  individually large. A hundred medium results overflowing the cap are left alone (one
  stderr line, no content change): a file per result would cost more than the context it
  saves.
  → `joy-rs/joyczl-loop/src/budget.rs`
* **Deliberate** — Calibration is a **ratio** (observed / estimated) clamped to 0.5–2.0,
  so one odd request cannot drag the budget off. Anything sharper means a real tokenizer
  per provider family.
  → `joy-rs/joyczl-state/src/chat.rs` (`context_factor`)

## Memory

* **Gap** — No dedup or merge on write. `facts` has no unique constraint and
  `add` always inserts, so repeated `save_note` calls or re-distilled
  consolidation can accumulate near-duplicates. A merge pass (or a uniqueness
  rule on subject+content) would go here.
  → `joy-rs/joyczl-state/src/facts.rs`, `joy-rs/joyczl-state/migrations/`
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
  (python3, a working sandbox) declare `prereq` and are skipped — counted as
  skipped, never as passed.
  → `evals/deterministic/exec.jsonl`
* **Gap** — The embedding path has no end-to-end test against a real embedding
  server: fusion and degradation are unit-tested, the HTTP call itself is not.
  → `joy-rs/joyczl-memory/src/retrieval_tests.rs`

## Configuration

* **Deliberate** — `config/write` persists to `<home>/settings.json`, which is
  applied *on top of* the environment. A saved patch therefore outranks
  `JOY_*` for that field until it is cleared (empty string clears model
  overrides; an empty patch removes the file).
  → `joy-rs/joyczl-config/src/lib.rs` (`apply_patch`, `save_patch`)
