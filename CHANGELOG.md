# Changelog

English | [简体中文](CHANGELOG.zh.md)

## Unreleased

### History display strips the model-only tool fold

- Stored assistant replies carry a `\n[tools used: …]` trailer — written by
  `fold_tool_activity` for the **model** (so the next turn remembers what was
  already done). The human read it too, and wondered why a home-directory
  listing was in the chat. The display layer now takes the fold off
  (`stripToolFold` in `markdown.ts`): last occurrence wins (tool outputs may
  quote the marker), and it only strips when the text really ends with the
  fold's closing bracket. The fold stays in the database — one stored text,
  two renderings. Live completion replies were always clean.

### Sidebar session list

- Session rows become **exactly one line**, sidebars: title on the left (ellipsis when
  it overflows), relative time on the right, nothing else — details (message count, session id) 
  live in the tooltip. No card box: hover and the current selection are the only backgrounds, 
  the selected row gets a 2px accent bar on the left. Arrow keys move the focus along the rows
  (Enter activates — native to buttons).
- `sessionList` no longer nests a `ul.list` inside the host `<ul class="session-list">`
  — it returns `<li>` rows directly, one list. The overview's "最近会话" section shares
  the same rows (one source of truth, two surfaces, as before).
- Service worker `VERSION` bumps to `joy-v14`: `main.js` is cache-first, so a frontend
  change without a bump keeps serving the old bundle — exactly why the first version
  of this redesign looked like "nothing changed".
- The sidebar's **edge is draggable**: grab the border, pull left/right, width sticks
  in localStorage (180–480px, default 232). Double-click resets; ←/→ nudge by 24px
  when the handle is focused; a 7px hit area rides the border with an accent line on
  hover/drag; pointer capture keeps the drag alive when the cursor outruns the handle.
  Hidden on mobile, where the sidebar turns into a top bar.
- Truncated titles get the **hover marquee**: hover for 500ms and the title
  scrolls horizontally (40px/s, ellipsis lifted while scrolling), holds a beat at
  the end, scrolls back, repeats while the pointer stays; leaving snaps back
  instantly. Titles that fit never move; `prefers-reduced-motion` users never
  see movement at all (the tooltip still carries the full text). The title is
  now two spans — the outer clips at the row edge, the inner carries the
  ellipsis and the translate.

### Dashboard streams Markdown as it arrives (block-incremental paint)

- The chat bubble no longer shows raw `**` during a turn and flips to rendered
  text only when the stream ends. A new `StreamMarkdown` (in `markdown.ts`)
  paints the reply **while it streams**, at block granularity: every animation
  frame re-parses with the same `parseMarkdown` but repaints only the
  still-growing last block — settled blocks keep their DOM nodes, so a
  several-thousand-token reply never reflows per delta. Half-finished inline
  markers (`**`, open fences) heal on the next frame.
- The parser has no strict prefix stability for half-line prefixes (a stream
  cut inside a `- ` marker parses the lone `-` as a paragraph until it merges
  back into the list), so each repaint reconciles cached per-block signatures
  and repaints from the first divergence — normally an all-equal O(reply)
  comparison, same order as the parse itself.
- `finish()` completes the last block; the truncated-turn branch no longer
  repaints the whole bubble. The one full repaint left is `completed.reply` —
  the authoritative result whose deltas may be missing. Node `--test` gains a
  streaming-invariant test over whole-line prefixes (111 tests).

### Provider catalog meets models.dev (`scripts/gen_catalog.py`)

- The two halves of "add a provider without touching code" are now complete:
  the runtime already had `<home>/models-catalog.json` (patch/append, merged
  at boot); this round adds the generation side — `scripts/gen_catalog.py`
  pulls model metadata from [models.dev](https://models.dev) (MIT, maintained
  by the OpenCode team) and emits catalog-format rows: context window, vision
  and tool-call come from upstream; wire and key_env come from joy's mapping
  of the 12 built-ins (gemini→google, kimi→moonshotai, glm→zhipuai,
  opencode_go→opencode-go, …). Stdlib only.
- The script does not decide for you: the default model is "the
  largest-context tool-call model", with candidates printed to stderr for a
  human look; `--out` merges into the file (dedup by id), without it the row
  goes to stdout. See "The model catalog" in docs/configuration.md.

### Windows keychain + the `SecretVault` seam

- `joyczl-config` gains a Credential Manager backend on Windows (the `keyring`
  crate, CredWrite/CredRead): keys no longer can only land in plaintext
  `secrets.json`. The 5-second timeout, the fall-back-to-file and the
  "report the real landing spot" discipline match macOS/Linux; the shipped
  paths there (`security` / `secret-tool`) are untouched. The Windows branch is
  compile-verified via `cargo check --target x86_64-pc-windows-msvc`.
- New `SecretVault` trait (`put`/`get`/`remove`, with put required to report
  where the secret really landed); `SecretStore` is the first implementation,
  and `apply_profile` / `resolve_key` now take `&mut dyn SecretVault` — when
  the server shape brings a KMS-envelope backend it slots in without touching
  any caller. Connector and MCP token files are not migrated yet: their
  rotation/lock semantics (0600 + temp-file-rename + cross-process lock)
  deserve their own round, timed to when the server shape actually ships.
### Collapse: a goal is no longer an object — it lives on the task row

- Removed the protocol methods `goal/set` and `goal/view`, the types `GoalSetParams` /
  `GoalSetResponse`, the HTTP route `POST /api/goal`, and `Server.goals` — the "runner's mirror".
  The truth has been `agent_tasks.goal_condition` since 0.14; the mirror was a second copy restored
  at boot and kept in step by hand in several places, and two truths eventually diverge.
- The loop now reads the row: condition from `task.goal_condition`, rounds from `round_no` (written
  on the same row), terminal state from `TaskStatus` (`failed` + reason). No third state. `goal.rs`
  is down to the judge.
- One human door: `/goal <condition>` (creates the task if there is none, edits it if there is;
  `/goal clear` takes only the condition off and leaves the row as record). The protocol therefore
  has **no** way to set a goal — programs use `task/create {goalCondition}`.
- The dashboard's task card no longer reads `DashboardData.goal`: it was already reading
  `data.task` (the same row), and the extra "目标" block was duplicate modelling.
- Tests: `a_goal_is_the_task_row_and_nothing_else` asserts the row itself (set / edit the same row /
  clear the condition / an empty session creates nothing); the loop tests start through
  `set_task_goal`, and the round-limit assertion changed from "the goal is still in memory" to
  "the row is still there, `status = failed`, the condition was not cleared".

### Cleanup: three switches nobody read

- `experimental` (`JOY_EXPERIMENTAL`) is gone from `Settings` / `SettingsView` / `SettingsPatch`:
  it never had a consumer, yet the dashboard's overview showed it as "实验 · experimental".
- The `apple_calendar` / `google_calendar` settings fields are gone too: the calendar tool has always
  read `JOY_APPLE_CALENDAR` / `JOY_GOOGLE_CALENDAR` directly (`tools/src/calendar.rs`), so the
  settings copies were `config/write`-able and inert. **Protocol change**: `SettingsView` /
  `SettingsPatch` each lose three fields (TS / JSON Schema / Python SDK artifacts regenerated).
- The dashboard's "实验" row disappears with them — all three bits it printed did nothing.

### Cleanup: dashboard painters with no page

- Removed `renderUsage` / `renderApprovals` / `renderSchedules`, and from `api.ts`
  `fetchUsage` / `fetchApprovals` / `fetchSchedules` / `fetchTasks` / `fetchModelProfiles` /
  `fetchModelRoutes` / `saveModel` / `saveModelRoutes` / `removeModel`: after the config panels left
  the dashboard these had no callers and were dead source that esbuild tree-shook anyway. The HTTP
  endpoints stay (the CLI and hub still use them).

### Docs

- `joy --help` now lists `joy bench`: it has been there all along (long-task benchmark; records
  success and recovery rates, never blocks a release), it had just never been in the usage text.

## 0.16.0 — One turn per session at a time (session gate + steering)

### The session gate

- `turn/start` claims the session first: if a turn is already running on that `session_id`, it returns
  `SESSION_BUSY` (-32003) with the holder's `turnId` in the message — that one id decides whether you
  wait or steer.
- The claim happens **before any side effect**: a refused turn does not flip a task to `running` and
  leaves no checkpoint behind.
- Resume goes through the same gate: two checkpoints of one session never recover side by side; the
  one that loses the race stays `open` for the next boot.
- The gate has no storage of its own — it is an invariant on the `turns` table, so there is no second
  copy of the truth to drift.

### Steering

- New `turn/steer { sessionId, message }`: adds a sentence to the turn that is **already running**;
  it enters the working memory at the next iteration boundary. Tools that already ran are not
  re-executed — that is the whole difference from cancel-and-retry.
- All three outcomes are stated: idle session → `INVALID_PARAMS` pointing at `turn/start` (it does
  **not** quietly start a new turn); a second addition → `queued: 2`; the sentence is always persisted.
- `chat_log`: an addition is its own `user` row (`meta.steer`), shown where it happened;
  `session_history` folds consecutive user rows back into one ask, so the model still sees a
  question/answer pair.
- New `turnSteered` notification: every other client of that turn (second device, REPL) learns about
  the addition immediately.
- Dashboard: pressing send while a turn runs now steers; sending with attachments says "they wait for
  the next turn" out loud (that request has already gone out).

### Queuing the next turn (follow-up)

- New method `turn/follow-up { sessionId, message }`: while a turn is still running, queue a line
  for "do this once you're done". The line between it and `steer` is **ownership**: steer belongs
  to this turn, follow-up belongs to the next one.
- The queue is **a table** (migration 0019 `session_queue`), not a Vec in the server: a queued
  instruction is a promise to the person who typed it, and a restart must not quietly drop it.
  No status column — a row exists exactly while it has not been handed to a turn
  (claim-then-run); once it runs, chat_log is the record.
- One at a time: the head is taken when a turn ends, so the queue never fights the session gate.
- Called on an idle session → `INVALID_PARAMS` pointing at `turn/start` ("wait for this turn"
  must not be reinterpreted as "start one now"); success returns `queued` (your position), and a
  write failure says it was not queued.

### Known

- The gate is **per process**: two app-servers on the same home still know nothing about each other
  (and `turn/steer` only finds turns in its own process).
- follow-up notifications still follow "one SSE stream = one turn": a queued line only appears in
  a stream once the next turn starts.

## 0.15.0 — A session has a row of its own (session metadata)

### A session is no longer just a label

- Migration 0018 adds `sessions` (`id` / `title` / `cwd` / `created_at` / `last_at`); existing
  conversations are backfilled from `chat_log`. Additive only — no existing row is touched.
- `session/new` now really writes: an empty session shows up in the list immediately, with a
  name and a bound working directory. A blank `title` still falls back to the first user
  message on the read side, exactly as before 0.14.
- New `session/get` and `session/update` (rename / rebind the directory; `null` = leave alone,
  `""` = clear). Both are **human-only**, like `task/*`: the model may not rename its own
  conversation or move its own workspace.
- New `turn/list`: which turns are running, optionally for one session. Each `turns` entry
  carries `session_id` and a start time — a table keyed only by `turn_id` cannot answer "is this
  conversation running right now".
- Writing a message creates the session row, so REPL / gateway / scheduled conversations are
  listable too: there is no longer such a thing as "a conversation nobody can find".

### The working directory travels with the session

- `turn/start` without `cwd` uses `sessions.cwd` (precedence: this turn → the task's own
  directory → what the session remembers). If that directory has since been deleted the turn
  runs with **no** writable root and says so — it never quietly falls back to the process cwd.
- The writable boundary is still that directory itself: `JOY_EXEC_WRITABLE_ROOTS` are extra
  roots, home is not one.

### Fixed

- The `eval` recipe in `just check`: cases load by relative path, so `cd joy-rs` first always
  died with `No such file or directory`.
- The Python SDK now exports the task types 0.14 had left out, plus response pairings for the
  three new methods.

### Known

- Work sessions / shared team spaces (members, roles, handover, promoting a personal session)
  are **suspended**; the design is in `docs/planning/user-center.zh.md`. No multi-user code has
  landed.
- Two concurrent turns in one session still have no serialization guard (that is 0.16).

## 0.14.0 — Done means measured (the task-entity version)

### Tasks stand on their own

- Migration 0017 rebuilds `agent_tasks`: `session_id` is nullable (a task may belong to no
  conversation at all), the "one active task per session" partial unique index is gone, and the
  row grows acceptance conditions, a budget, what has been spent, its own working directory and
  the reason it last stopped. A destructive migration on purpose — old rows are not converted.
- Four new tables around it, each with one job: `task_runs` (one row per execution — "executed
  3 times" is a count, not a guess), `task_steps` (one row per tool call, with an idempotency
  key `<turn_id>:<seq>`), `task_events` (the readable timeline), `task_artifacts` (the files
  that really exist). `turn_checkpoints` gains `run_id`, `chat_log` gains `task_id`.
- `task/get`, `task/runs`, `task/artifacts` join the human-only task methods.

### "Done" is decided by facts

- `verify.rs`: three deterministic checks — `fileExists`, `fileContains` (a plain substring,
  never a regex: a broken pattern must not stall a round), `toolsOk` (counted from the step
  ledger, not from the model's summary). Every round is measured before the judge is asked, and
  **a failing condition skips the judge entirely** — a model must not be asked to confirm a
  fact that is already false.
- `observe.rs`: the tool's claim is paired with the filesystem. "Said it wrote, the file is not
  there" becomes a contradiction, the one signal that stops a task from reaching `done` on a
  lie. Path containment reuses the write tools' rule (`inside_workspace` is now public and
  `contained` delegates to it) — one implementation, no second opinion; no workspace means no
  path check at all, never a fallback to home.
- New state `needs_attention`: non-terminal on purpose — a machine measured something it must
  not overrule, so a human looks (continue or cancel). Budget overruns land there too, naming
  the dimension that ran out.
- Task-level budgets (wall clock / tokens / tool calls) are checked once per round, before the
  next model call can spend more.
- Crash safety: a step left `running` means "sent, the result never landed". Boot turns it into
  `waiting` — **never replayed**; a write that may already have happened is a human's call.

### Dashboard

- The task card grows three blocks: the acceptance checklist (tick / cross per condition, with
  the measured result beside a failure), the deliverables (files with real sizes), and three
  usage bars (only dimensions with a cap get a bar; 80% turns amber, 100% red). A footnote says
  "executed N times · last stopped at …". `needs_attention` gets its own warning pill.
  `sw.js` goes to `joy-v5`.

### Evals and gates

- `evals/deterministic/task-acceptance.jsonl`: a false "done", an over-budget stop, and a real
  file passing — all offline and reproducible. The eval harness can now create a task before a
  case's turns and assert its final status, verdicts and artifact count.
- `smoke-resume.sh` plants a task + run + a "sent but never landed" step and asserts the fresh
  process turns the step into `waiting` and closes the task honestly.
- `smoke-dashboard.sh` checks the new fields end to end (and that a second task in one session
  is now created instead of refused); `smoke.sh`'s hub section picks a free port instead of
  hardcoding 7801.
- `joy bench` + `evals/long/tasks.jsonl`: 10 long-task benchmarks (multi-round writes, cross-round
  `toolsOk`, write-then-exact-edit, nested folders, and 3 interrupt recoveries), reporting **task
  success rate** and **interrupt recovery rate** separately (interrupt → the process really
  restarts → a later turn finishes). It **records, never blocks**; the report also lands in
  `eval_report_long.json` so a benchmark run cannot overwrite the release gate's verdict.
  The benchmarks paid for themselves on the first run — two real defects, both fixed with a
  regression test each: step idempotency keys collided across rounds (round 2's write was silently
  dropped, so `toolsOk` never counted past one), and an interrupt kept the task loop burning
  rounds (the user's stop only took effect when the round cap ran out).
- Eval scenarios gained inline `{"gate": …}` replies (every round re-asks the gate) and
  `restart: true` (run the next turn on a fresh process), which is what lets the benchmarks
  measure restart-and-continue instead of assuming it.

### Execution plans (P1, `JOY_PLAN=1`)

- A long task may spend one extra model call before its first round asking for a 3–6 step plan,
  written into `task_steps` (`kind = "plan"`, empty `run_id` — a plan outlives the execution that
  drew it) and appended to that round's message as plain text. A failed round may revise the plan
  **once** (two versions max); a plan never carries acceptance conditions, so a model cannot
  soften the human's standard by re-planning. Off by default: one more model call and a changed
  prompt is a capability pressed by hand.
- A broken planner never takes the task down: no valid JSON means one stderr line and the round
  runs without a plan.
- New `joyczl-agent` crate: acceptance (`verify`), observation (`observe`), plans (`plan`) and the
  repo's single path-containment rule (`paths::inside_workspace`) move out of the app server; the
  write tools re-export the same rule (`tools → agent`, no cycle). The crate has no JSON-RPC and
  holds no `Server`, so fact-deciding logic is testable on its own.

## 0.13.0 — Someone knocks (the workspace version)

### Inbound mail: the door opening the other way

- A hand-written IMAP receiver (LOGIN / SELECT / UID SEARCH / UID FETCH; 993 implicit
  TLS, 143 STARTTLS, anything else plaintext) — the same reason as SMTP: four commands
  are wanted, not a mail suite. Fetching always uses `BODY.PEEK[]`: Joy glancing at a
  mailbox never marks another person's mail as read.
- The ledger (migration 0015): `connector_inbox` dedupes on (connector, mailbox, uid),
  `connector_cursors` keeps the high-water mark separately. **The first run only records
  the water level, it does not pull history** — those few hundred old messages are not
  "new". Bodies are stored as decoded plain text (text/plain first, text/html stripped as
  a fallback); attachments are not stored: what arrived is "someone wrote to you", not
  "someone sent you files".
- Two ways it becomes work, both written by the human: `imap.mode: "notify"` (default)
  records + notifies, and the dashboard's inbox card button (`inbox/summarize`) is the
  human saying "handle these now"; `"turn"` is a standing authorization, never the
  model's own decision. Polling has a 30-second floor, and a typo'd `mode` fails startup.
- The dashboard overview grows an inbox card (unread count + newest five); REPL prints
  one line "N new in the inbox" between turns, leaving the content to the panel.
- `smoke-connector.sh` gains section 6: shipped binary → poller → scripted IMAP →
  `state.db` → `inbox/list`, across two boots and the cursor (old mail not repeated,
  new mail arriving once).

### Feishu user identity: as whom the message is sent

- The app identity says "this app wrote to you"; a user token says "**I** wrote to you".
  `feishu_oauth` runs the authorization-code grant (no PKCE), catches `code` on a local
  `127.0.0.1:41766/callback`, stores the token in `<home>/connector-auth/feishu.json`
  (0600), and refreshes before use. `joy connector feishu login | logout | status`;
  `"as": "user"` in `connectors.json` sends as that person, the default stays `app` —
  identity is the human's decision, and a typo fails at startup.

### Business form (M5): wiring Joy into someone else's process

- `POST /api/run`: one turn in, one JSON answer out (`turnId` / `reply` / `model` /
  `provider` / `routeReason`), body is `TurnStartParams`. The dashboard keeps SSE for
  people; CI, cron and other agents use this.
- Inbound webhooks (`<home>/webhooks.json`): no file = no door (404); `X-Joy-Secret` is
  compared in constant time and an unreadable secret refuses; unknown names and
  `enabled: false` are also 404; `{{payload}}` is replaced by the raw body. The turn is
  still an ordinary turn — with nobody at the keyboard, destructive tools time out.
- A task can declare its tier: `task/create` and `turn/start` accept `tier`
  (`agent_tasks.tier`, migration 0016), and **a declaration beats `fixed` mode**; a typo
  is refused on the spot. New `model/routes` / `model/routes/save`, and the dashboard's
  model panel gains an auto switch plus three "tier ↔ profile" rows.

### Webhook phase 2: what a door needs once strangers knock

- `signature: true`: `X-Joy-Timestamp` plus
  `X-Joy-Signature` = `hex(HMAC-SHA256(secret, "<timestamp>.<body>"))`; more than ±300s
  from our clock → 401, and each signature is accepted once (replay → 409; retries must
  be re-signed). The format is pinned against `openssl dgst -sha256 -hmac`, so the other
  side installs nothing.
- `limit: N`: N accepted requests per minute per hook, counting **only authenticated**
  ones (guessing secrets does not burn the budget), over the cap → 429 +
  `Retry-After`; `limit: 0` is refused at load.
- `callback: "https://…"`: the caller gets `202` at once, the turn finishes in the
  background and the result is POSTed (three attempts, 1s/2s apart, no queue).
- New `smoke-business.sh` (task API / three webhook modes / tiers) and
  `fake_callback_server.py`; `fake_sse_server.py` now serves several turns.

## 0.12.0 — The hand that reaches out (the workspace version)

### Connectors: the one exception to "draft, never send"

- New `joyczl-connector` crate: email (SMTP, 465 implicit TLS / 587 STARTTLS) and
  Feishu (app identity, `tenant_access_token`, cached in memory for two hours). No SMTP
  client suite, no native-tls — only the lines this feature needs.
- **Three fences, all required**: no `connectors.json` → not even the tool names are
  registered; configured still means `live: false` drafts to `outbox/`; every live send
  raises one approval card (recipients + body preview). Empty allowlist = nobody,
  failing closed.
- Credentials go through the existing `SecretStore` (keychain first, 0600 plaintext
  fallback, reported honestly); `connectors.json` holds intent only. A typo'd key fails
  at startup rather than silently meaning "nobody".
- `joy connector list / email / feishu`: the human's direct path, sharing the same
  allowlist and the same send code as the model (terminal sends skip the card — the
  person typing *is* the approval).
- The dashboard grows a read-only connector panel: which connector, whether it can
  really send, how wide the allowlist is; the switches stay in the file. Also fixes a
  crash when there is no task (`task: null` treated as an object).
- New `smoke-connector.sh`: the shipped binary against a fake SMTP server and a fake
  Feishu endpoint — no config / outside allowlist / draft / real send (RFC 2047
  subject, base64 body, receive_id shape).

## 0.11.0 — A task has an identity (the workspace version)

### Long tasks as entities: the goal loop becomes their engine

- A goal used to be a string in process memory: gone on restart, and "pause" was
  meaningless. It is now a row in `agent_tasks` (migration 0014): id, name, status,
  round account. `goal/set` becomes the compatibility layer — setting a goal creates
  the task, clearing it downgrades the task to single-turn; `/goal` and the task card
  point at one table from now on.
- **One active task per session**: a partial unique index as the backstop; a second
  `task/create` is refused with the existing one named — the human should see what
  they already started, not a stack of ghosts.
- State machine: pending / running / suspended / done / failed / cancelled. The judge
  says satisfied → done; impossible / round-limit → failed (honestly recorded as not
  done, never faked); judge broke down → suspended (stopped in place, the human decides).
- **A pause is not a stop**: `task/suspend` interrupts the turn but marks its
  checkpoint `suspended` instead of `abandoned`; `task/resume` continues from that
  boundary, finished tools never re-run. `task/cancel` is the stop — checkpoint
  `abandoned`, never auto-resumed.
- Boot reconcile: a `running` task is marked `suspended` (with the reason), and an
  `open` checkpoint resumes and flips it back to running. **A human's pause is never
  undone automatically.**
- The event bus grows a `task` ping (distilled from `TaskUpdated`); the dashboard's
  task card shows status, rounds and the suspend reason, with pause / continue /
  cancel switches — the only place those exist.
- Smoke extended: create → visible on the first screen → second refused → pause →
  resume → cancel, the whole chain over real HTTP.

## 0.10.0 — Crashed? Continue. (the workspace version)

### Turn-level checkpoints: the long-task engine's foundation

- A turn's working memory (model thoughts, tool calls, their results) used to
  live only in process memory — the process dies, the whole turn starts over.
  It now lands at every **tool-results boundary** (migration 0013
  `turn_checkpoints`); each boundary overwrites the previous one, newest wins.
- **Startup reconcile resumes**: a fresh process finding an `open` checkpoint
  continues the turn — the turn the human asked for — never re-running finished
  tools; the pre-crash token account joins the resumed one. A `turnResumed`
  notification carries the completed iteration count; nothing is silent.
- A resumed turn shares the **same tail** as a fresh one (`finalize_turn`):
  trace, usage ledger, chat log, consolidation — one road, no second copy to
  forget.
- The gates don't move: resuming re-enters checkpointed, already-approved
  territory; new tool executions still pass the approval gate (default-deny).
  A human interrupt and an in-process error mark `abandoned`, never auto-resumed
  — after a process ends, `open` can only mean a crash.
- Three new knocks on the event bus: `task_progress` / `task_resumed` /
  `task_suspended` — still lightweight pings, still one connection, reusing
  0.9.0's coalescing and reconnect.
- End-to-end smoke (`scripts/smoke-resume.sh`): boot a real process, plant the
  crash leftover, kill -9, restart — the reconcile's stdout frames and the
  ledger closure are asserted line by line.

## 0.9.0 — Accounts and every screen (the workspace version)

### Hub: register / login

- `POST /api/auth/register|login` — the only `/api/*` outside the authorize
  layer. Passwords are PBKDF2-HMAC-SHA256 (ring — already in the tree via
  rustls), salted, constant-time verified; unknown users get the same verdict
  and the same wall-clock as wrong passwords.
- users.json v2: `user → { credential, devices: {name → token} }`. **登录即
  配对** — logging in from a device pairs it; same device + password returns
  the same token. The flat v1 file migrates on load.
- Seats: `JOY_HUB_MAX_OPEN` doubles as the user cap. The first user must
  register from loopback or via `joy hub adduser` — a fresh public hub never
  lets a stranger claim it. Empty users.json now opens.

### Dashboard: a client of any Joy server

- Server address configurable; login card (server / username / password /
  device) replaces the whole view until authed. Remote 401s open the card
  ("session expired"); same-origin token deployments keep the one-time prompt.
- Verified against a live hub in a real browser: register → token → app boots
  pinned to that user's isolated home.

### Cross-device sync

- `GET /api/events`: one SSE channel of pings per app-server — {kind, turnId,
  sessionId} for turns, approvals, memory consolidation, errors. Pings are
  knocks; the client re-pulls `/api/data`. 500 ms coalescing; reconnect
  forever. The connection pins the user's app-server (warm by design).
- `CorsLayer::permissive()` on `/api/*` — clients authenticate by header,
  never cookies, so cross-origin adds no CSRF surface.

### PWA + Tauri desktop

- PWA: manifest, SVG icons (brand wordmark, maskable), service worker that
  caches static assets and never touches `/api` — installable on phones.
- Tauri 2 shell (`desktop/src-tauri`, its own workspace): one window, the
  bundled dashboard, the login card points it anywhere. `just desktop-dev` /
  `desktop-build`.

### Docs

- configuration (+zh): the hub chapter rewritten around accounts, pairing,
  seats and the deployment matrix; limitations (+zh): what bare accounts don't
  do (recovery, rate limits, 2FA) and why sync is a knock, not a feed.

## 0.8.0 — Write, see, declare, ship (the workspace version; `v0.5.0` is the previous tagged release)

### File-writing tools: `write_file` / `edit`

- **Off by default**; `JOY_WRITE=1` turns them on — the same human-switch
  philosophy as `JOY_EXEC`. Approval semantics are the ones `run_command`
  already has (`JOY_APPROVAL` three modes via `ApprovalBroker`).
- `edit` demands an **exact** `old_string` match: zero or multiple hits are a
  refusal, not a guess; the reply carries a unified diff. Binary files are
  refused (NUL sniffing), as are paths outside the sandbox like `run_command`.
- Rejections come back as text the model can read and act on — `execute`
  never errors, the standing invariant holds.

### Image input, end to end

- `ContentBlock::Image` in the provider layer; both wires speak it (Anthropic
  image block; OpenAI content array with a data URI).
- Providers without a `vision` flag fail with an **explicit** error — never a
  silently dropped attachment.
- `turn/start` attachments and the dashboard upload embed images within a
  size cap; token estimation counts the bytes.

### Model catalog

- A built-in catalog (12 providers, context windows and metadata) is compiled
  into the binary; `<home>/models-catalog.json` overrides/merges by id. No
  network, no new dependency, merged once at boot.
- `model/list` and the dashboard model panel read the merged catalog.

### Declarative workflows

- triage / gather topology moved to **JSON DAGs**
  (`joy-rs/joyczl-graph/src/workflows/definitions/`), validated at load:
  static edges must be acyclic, routers must be **registered built-ins**
  (`key_router` / `needs_action` — the whitelist is closed), and every node
  must have a bound implementation. Control flow can no longer reach the
  model even by editing the JSON. Workflows are compiled in
  (`include_str!`) — they are behaviour, not runtime config.

### hooks: HTTP handlers

- A hook entry now takes `command` (shell) **or** `url` (HTTP POST carrying
  the event payload). The decision contract is identical to shell exit codes:
  HTTP 200 + decision JSON blocks/allows exactly like exit 0/2. Existing
  `command`-only configs are unchanged.

### Per-session write-section lock

- One tokio Mutex per session serialises **only the write sections** of a
  turn (history append, consolidation, compaction); model calls never hold
  it, so a slow turn cannot stall the session and `turn/interrupt` still
  gets in. The three SQLite-level guards stay — they stop double-write
  corruption; the lock stops read/write interleaving.

### Platform boundary: Windows

- The CLI and the server **refuse to start** on Windows with an explicit
  reason (no sandbox, no execution, no pretending). Support matrix in
  `docs/operations.md`; CI keeps a Windows build+test job.

### Distribution materials

- npm shim package (`joyczl-agent`: downloads the platform binary from
  GitHub Releases), Homebrew formula hash automation (`just release-hash`),
  a tag-triggered release workflow (four binaries + per-platform wheels),
  and a RELEASING manual (EN/zh). **Publishing is a human step** — tokens
  do not live in CI.

### Docs

- Fixed the broken README tool table, the empty limitations sections and a
  stale memory-de-dup entry (all EN/zh pairs).

## 0.7.0 — Aligning with the state of the art (the workspace version; `v0.5.0` is the previous tagged release)

### Web search: provider chain, per-turn cap, untrusted wrapping

- **Ordered provider chain**: Tavily -> Brave (`BRAVE_API_KEY`, new) -> DuckDuckGo;
  DuckDuckGo enters the plan only when no key is configured — after a keyed
  backend fails, the free endpoint stays out of the plan.
- **No backend, no tool**: with no key and `JOY_SEARCH_DDG=0`, `search_web` is
  not registered at all (a promise that cannot run only invites guaranteed
  failures); startup says so explicitly.
- **Per-turn cap**: `JOY_SEARCH_MAX_PER_TURN` (default 20); the budget check
  short-circuits before any network — a loop that varies its query each time is
  invisible to the loop guard, but not to the budget.
- **Search results wrapped as untrusted**: a poisoned title/snippet is the same
  injection threat as a poisoned page, just one tool call earlier; hits go
  through the same "data, not instructions" paraphrase pipeline as fetched bodies.

### A stale dashboard frontend now refuses to start

The dist is a build artifact and gitignored — nothing ever forced a rebuild,
so a source tree a month ahead of the served UI could pass as perfectly
healthy (server 200, user looking at last month's page). `joy dashboard` and
`joy hub` now check freshness at startup: **if the sources are newer than the
dist, it's a hard error** with the rebuild command in the message. Same
philosophy as the daemon lock: misconfigured should fail to boot, not silently
serve something stale. Release scenarios (no joy-ts sources present) skip the
check.

### Folder picking and recents for the working directory

- **Visual folder picking** :
  server-side browsing via `GET /api/fs?path=<dir>&hidden=1` (the browser's
  picker can't hand over absolute paths — the filesystem-aware side must do it) plus a dashboard overlay (breadcrumbs where
  every crumb is a jump target, "＋ 新建文件夹" via `POST /api/fs/mkdir`
  with a whitelist-checked name, dot-prefixed dirs hidden by default with a
  toggle, build noise always hidden).
- **Recents**: "directories you visit often don't need re-selecting".
  `<home>/workspaces.json` with canonicalize dedup (symlink spellings and
  trailing slashes don't create duplicates), most-recent-first, capped at 8;
  two recording points — when a turn carries a validated cwd, and when the
  dashboard's "选这个目录" is clicked (intent is expressed at selection
  time, no need to wait for the turn). A "最近使用" block at the top of the
  picker jumps in one click. `GET/POST /api/fs/recents` (ops-local view,
  not in the protocol).

### Automatic per-task model routing

A tier abstraction ("You don't pick the model, the task does.")
plus an "auto" feel:

- `<home>/model-routes.json`: three tiers `fast`/`chat`/`reasoning`, each
  pointing at an existing profile id; `fixed` is the default (zero behavior
  change). The file is re-read every turn — edits take effect immediately.
- L0 rules at zero cost: token estimate over half the chat profile's window
  → reasoning. The threshold follows the provider's window — no new knob.
- L1 classifier (only in the ambiguous band ≥2k and under half): asks the
  cheap small-model once "is this deep reasoning?", fail-open to L0.
- Resolution failures fall back to the active profile and say so in
  `TurnMeta.routeReason` — routing never breaks a turn.
- `JOY_MODEL_ROUTE=auto|fixed` forces the mode; REPL `/route`; the
  dashboard footer prints "路由 auto:reasoning → ds-r1".
- Discipline: routing is a server-side policy — the model has no tool to
  switch its own model; the human's active profile always wins.

### Three interactions: working directory, file upload, model quick-switch

Assembling joy's minimal version from the best-in-class interactions
observed across reference harnesses:

- **Session working directory**: `TurnStartParams.cwd` (per-turn
  override semantics) → `ToolCtx.cwd`. Admission is fail-closed (must be an
  absolute path that exists); exec's fallback chain is
  `args.cwd > cwd > process cwd`, and `read_file`/`list_dir` resolve relative
  paths against it; the cwd joins exec's
  writable roots; subagents inherit the parent
  turn's cwd. Entries: REPL `/cwd` and the dashboard header.
- **File upload**: `POST /api/upload` (octet-stream, single file — no multipart
  parser; 20MB cap). The on-disk name is fully server-generated (only the
  extension is taken from the user, whitelist-checked — user input never
  enters the path, same discipline as spill); the frontend stages files, then
  uploads on send and injects absolute paths into the message — the model
  reads them with `read_file` (reference-first). Images are not embedded;
  see limitations.
- **Model quick-switch**: the header model pill becomes a dropdown, sharing the same
  `saveModel({id, makeActive})` path as the
  model panel — persistent config, without a session-vs-global split
  (joy's profiles are persistent by design).

### Dashboard redesign: run status, tool cards, data-as-navigation

Run status, tool cards, navigation and transcript rendering — absorbed in
vanilla TS (no framework, no new dependencies):

- **Three-state run status**: waiting-on-you (a still line) / thinking
  (shimmer + stopwatch) / working (tool name + stopwatch) — separates
  "stuck" from "running".
- **Tool cards**: each tool extracts one human summary from args
  (run_command shows the command, read_file the path, fetch_url the URL),
  with a live stopwatch and the final outcome; failures are visible
  immediately (previously a raw JSON chip).
- **Right panel becomes tabs (data as navigation)**: overview / memory /
  **usage** / **approvals** / **schedules** / model — the M2d usage ledger,
  M3b audit, and M4b schedule state finally have UI; fetched on demand
  (only the active tab).
- New protocol method `schedule/list` (no args; per-job last_fired/claimed/
  catch_up/delivery snapshot); ops gains three read-only routes
  (`GET /api/usage`, `/api/approvals`, `/api/schedules`).
- Two-layer design tokens: a material layer (:root variables) plus a
  semantic layer; dark mode switches one variable set; the warm-orange
  identity stays.
- Fixed a guaranteed-crash bug along the way: apiFetch's 401 retry called
  itself instead of fetch — any 401 would have blown the stack.

### M4c: a scheduler run ledger — "what ran, and how it ended" has an answer

- `<home>/schedule-runs.jsonl`: one JSON line per run with three
  first-class outcomes — `ok` / `empty` / `error`. Errors previously lived
  only in stderr, outbox was only written on success (quiet wrote nothing),
  so there was no single place to inspect runs.
- **Deliberately no per-job iteration cap**: scheduled turns inherit the
  global `JOY_MAX_ITERATIONS`, subagents have their own — a per-job
  override would touch the protocol types (TurnStartParams → generated
  artifacts in three languages) with no real consumer today. YAGNI until a
  job actually hits the ceiling.

### M4b: scheduler upgrade — state into SQLite, replay protection, per-job delivery

- Migration 0012 `schedule_state`: dedup (last_fired), crash recovery
  (claimed), and per-job policy (catch_up/delivery) in one table — the old
  schedules-state.json is now only a one-time import source, no longer
  authoritative.
- **Claim before dispatch** (advance next_run at
  dispatch): `claim` is persisted before the run, `confirm` after. A crash
  in between is explicitly handled at restart by the `catch_up` field:
  newsletters (=1) re-run once (at-least-once), notifications (=0) are
  dropped (at-most-once). **Per-job, never a global either/or.**
- **Per-job delivery** `delivery`: `outbox` (default) / `notify` (file +
  notifier, new ScheduleRan event) / `quiet` (log only). An empty reply or
  `NO_REPLY` skips delivery — output with no content isn't worth a ping.
- Failures confirm too: with no backoff machinery, a failed job must not be
  retried every 20 seconds.

### M4a: a hub resource pool — the minimal correct answer to density

- Two knobs: `JOY_HUB_MAX_OPEN` (default 32, how many per-user app-servers
  hub keeps open) and `JOY_HUB_IDLE_SECS` (default 900, close after idle;
  0 disables the policy).
- Three strategies for the pool: a **cap**,
  **LRU/idle eviction**, and **never evict in-use instances**. Eviction
  doesn't guess concurrency from timestamps — it uses `Arc::strong_count`:
  only instances held solely by the pool can be closed; anything serving a
  request is naturally pinned.
- The cap is **soft**: when everything is pinned, the pool over-commits
  rather than turning requests into errors — the overshoot is real
  concurrency, not idle accumulation.
- A bonus from the recon: comparable fleets also validate joy's "one home,
  one process per user" — they are process-per-user too, and admit
  it is not a production multi-tenant boundary.

### M3b: an approval audit — "who approved that rm -rf" has an answer now

- Migration 0011 `approval_log`: one row per "ask-before-run", with four
  outcomes — `approved` / `denied` / `timeout` (nobody answered) / `voided`
  (the turn ended; known the instant the oneshot sender is dropped).
- **A single write point**: the tail of the approval-waiting future — all four
  exits pass through it, so no path can forget to log. A write failure never
  blocks the main path (the ledger is a shadow; the causality lives in the
  oneshot channel).
- New protocol method `approval/log`: the last 100 records, newest first; a
  read failure returns an empty list with a stderr note.
- This information previously lived only in memory and stderr, gone when the
  turn ended — now a hub admin can audit it afterwards.

### M3a: a notifier — know when you're not at the screen

Three kinds of "a human should look at this" leave the process: **approval
requests** (come approve in the dashboard), **background-job finishes**
(completed/failed/killed/interrupted all reported), and **goal outcomes**
(satisfied included — a stopped autonomy loop deserves a heads-up either way).

- Config: `JOY_NOTIFY=1` + `<home>/notify.json` (`{"slack": webhook URL}`).
  The URL is admin-configured — **deliberately no SSRF guard**: the `fetch_url`
  guard exists because the model picks those URLs; an admin pointing
  notifications at a self-hosted intranet endpoint (ntfy/Gotify) is legitimate.
- **Sidecar discipline** (same as the usage ledger): fire-and-forget, 10s
  timeout, failures are one stderr line — the notification system breaking
  never blocks the turn.
- Implemented as an injected `Notifier` trait (same discipline as
  ApprovalBroker/JobRegistry); tests cover it with a capturing mock and a real
  loopback POST.

### M2d: a usage ledger (accounting only, no enforcement)

- Migration 0010 `usage_ledger`: one row per turn (session/provider/model/
  input/output); the `usage/summary` protocol method returns per-model
  aggregates and totals; a write failure **never blocks the turn** (the ledger
  is a sidecar) — one stderr line and on it goes.
- **Deliberately no cost column**: under BYOK the money shows up on each user's
  own provider bill; rates are the deployment's business (tokens are recorded,
  multiply whenever needed). An `estimated_cost_usd` column — that waits
  for platform-provided keys (M4); today it would be made-up numbers.
- In hub mode this is per-user accounting: one home, one ledger per user;
  cross-user aggregation belongs to an admin surface (M4).

### M2: multi-user on one machine (`joy hub`) — the shape teams can use

- **`joy hub adduser <name> --root <dir>`**: mints a token (getrandom, 64 hex,
  printed once), writes it into `<root>/users.json` (0600, tmp + rename).
- **`joy hub`**: one ops process serving every user; Bearer token → user → that
  user's home (`<root>/<name>/`), with the app-server child spawned **lazily on
  first use**. Each home is a complete set: state.db, models.json (**BYOK
  reuses the M1 work directly**), the jobs ledger, skills; the daemon lock was
  already per-home.
- **Isolation is structural**: user names are validated at load (lowercase
  letters, digits, hyphens; `../` cannot get in); two users' homes are disjoint
  directories — "A's keys never appear in B's paths" holds because there is no
  shared path, not because everyone remembered to be careful. Two tests pin
  this structure.
- The smoke suite gained a hub section: no token 401 / wrong token 401 / right
  token 200 / home actually created.
- **Deliberately not built** (none of the references have it): accounts and
  password stores, OAuth, per-user quota enforcement; pairing-code
  admission (an unauthenticated endpoint + rate limits) waits for a real need.

### M2 begins: an auth gate on the dashboard's HTTP surface

- **`JOY_OPS_TOKEN`**: a Bearer token for `/api/*`, compared in constant time;
  there is no "local requests bypass" — once a token is set, everyone carries it,
  otherwise the token only stops outsiders.
- **A non-loopback host requires a token**: binding `JOY_OPS_HOST` to `0.0.0.0`
  or a LAN IP without a token is refused at startup — a wrong config should fail
  to boot, not serve the LAN for ten minutes first.
- The token is deliberately absent from the `config/read` view: deployment
  parameters must not appear in payloads the model can see.
- The dashboard asks once for the token on first 401 and retries with it stored
  in localStorage.

### The groundwork for 7x24: job ledger, daemon lock, schedule catch-up

Studied how reference harnesses keep long tasks alive across crashes and ported
back three pieces — their answers agree so closely that the previously open
decisions are now settled:

- **Job ledger in the database** (migration 0009): background-job start/finish
  go into state.db and output streams to `<home>/jobs/<pid>-<id>.log` (1 MiB cap).
  After a crash, boot reconciliation marks still-running jobs `interrupted` and
  keeps output readable — **never auto-rerun** (unanimous across references:
  auto-rerun is an incident amplifier). `job_list` shows history, `job_output`
  reads dead jobs' output, and the owner fence covers history too;
- **Daemon lock**: one app-server per `<home>` (`app-server.lock` + PID;
  locks left by dead processes are reaped on next start). Two
  resident processes each holding in-memory state while writing one database is
  an incident, not a feature;
- **Schedule catch-up**: `joy schedule` persists its last-fired table (atomic
  write) and, at boot, looks back over the minutes since each job's last firing
  (capped at seven days), firing the latest missed slot **once**, marked as
  catch-up — missed firings are no
  longer silently swallowed.

### The model can see files now: read-only file tools

`read_file` / `list_dir` are **registered by default**. The only path to file
content used to be enabling `JOY_EXEC` and running `cat` — binding a
side-effect-free "have a look" to the execution gate. Now: pagination (2000
lines / 50KB, cut at line boundaries, never mid-line), line numbers, a
`[Use offset=N to continue.]` protocol, a "file unchanged" placeholder when the
same range is re-read in the same session, and explicit refusal of binary
files. Reads bypass the execution gate; `run_command` is unchanged.

`fetch_url` fetches a page's text (HTML→plain, 50k-char cap) behind an SSRF
guard: private / metadata / reserved targets are refused and **every redirect
hop is re-resolved and re-checked**. The body passes through the same
"data, not instructions" defusal as subagent reports — an injected page and an
injected subagent are the same threat, so they get the same defense.

### Breaking change: the old "set a key via environment variable" path is gone

Models used to be configured with `export JOY_API_KEY=…` (or `JOY_BASE_URL` /
`JOY_MODEL` / `JOY_SMALL_MODEL`, or a settings.json patch), and Joy would
**silently** fall back to the vendor's conventional variable for `JOY_PROVIDER`.
The result: "I never configured anything, yet it works because some variable
happens to be in my shell" — the hardest failure to diagnose.

Now there is **one path only**: the credential pointer declared by the active
profile in `<home>/models.json` (`--env VAR` is an explicit "read this one", not
an implicit fallback). Removed along with it:

- `JOY_API_KEY` / `JOY_BASE_URL` / `JOY_MODEL` / `JOY_SMALL_MODEL` are no longer
  read;
- the `config/write` patch no longer carries provider / model / small_model
  fields — models and keys move to `model/*`;
- `resolve()` no longer reads vendor environment variables implicitly; a missing
  key now prints the exact `/model add …` command;
- `JOY_PROVIDER` stays, but **only** as the fallback provider when no profile
  exists, so local models like ollama still start zero-config.

If you relied on environment variables, **your model is now unconfigured** — run
`/model add` once as the message says (the key lands in the OS keychain).

### Multiple models: 12 pre-adapted providers, switchable at will

The twelve providers were already pre-adapted in `joyczl-provider`; what was
missing was "configure it yourself, switch at will":

- **Several profiles at once**: `<home>/models.json` holds any number of
  "provider + model + endpoint + credential pointer" sets; `/model use <name>` (or
  the dashboard panel) switches — no need to retype the key, it rewrites in-process
  Settings and re-resolves the provider, **effective immediately, no restart**;
- **Keys go to the OS keychain first**: macOS `security` / Linux `secret-tool`,
  falling back to `<home>/secrets.json` (0600, plaintext); responses and the UI
  **say where the key actually landed**;
- **Human-only**: `model/profiles` / `model/save` / `model/remove` share one
  implementation with the REPL's `/model` and the dashboard panel; the model has
  no such tool and never sees a key.

### Video analysis skill: turn it into text first

`skills/video-transcription/SKILL.md` (bundled with the repo, and a living
example of the skill format): "analyze this video" → probe for yt-dlp / whisper
(honestly report when missing) → pull subtitles for online videos without
downloading, or transcribe local files → read the transcript with `read_file`.
Two disciplines are baked into the body: the model never describes video
content it has no text for, and subtitles are external data, not instructions.

### Concurrency: three guards where two turns meet

Two concurrent turns on the same session used to meet three read-then-act races:
consolidation read a batch, distilled it, then marked it, so both sides saw "due"
and distilled the same log twice, burning a model call; the rolling summary's
`ON CONFLICT … DO UPDATE` ignored which `covered_turns` was newer, so concurrent
compaction could hand the user an **older** summary; and `busy_timeout` read 3
seconds in the comment while the real value was sqlx's default 5 (`PRAGMA` is
per-connection and `execute(&pool)` only configures whichever connection served
that query).

Each is now closed at its own door: consolidation became a single
`UPDATE … RETURNING` **atomic claim**, so the loser gets an empty list and
**never calls the model** (failure paths release the claim — `consolidated` back
to 0 plus exponential backoff); the rolling summary gained a **monotonic guard**
that only accepts a summary covering more turns, and says so on stderr when it
refuses; and the lock wait moved onto the connect options at **10 seconds**,
SQLite's recommended value. Background job outbox filenames also carry the pid —
job ids are per-process, so two Joy processes used to write the same `j1.txt`.

No per-session lock: the cost (a visibly slower second turn) outweighs the gain
now that the three guards reduce the concurrency consequences to "waits a bit
longer". Tests: two concurrent consolidations assert the small model is called
once, a failed consolidation releases its claim, a rollup only moves forward, and
the PRAGMAs really are in effect — that last test caught "3 seconds in the
comment, 5 seconds in reality" on the spot.

### Goals: keep going until the thing is actually done

`goal/set` (and `/goal` in the REPL) attaches a condition to a session. When the main
loop stops, a judge — cheap model, no tools, conversation only — decides whether the
goal is met; if not, its reason becomes the next user message and another round runs
in the same session. Every judgement emits `GoalRound` (round, max, status, reason)
so the extra rounds are explained, and `TurnMeta.goalStatus` / `goalRounds` carry the
outcome into the turn's metadata.

Bounded and honest: `JOY_GOAL_MAX_ROUNDS` (default 5) caps it, and hitting the cap
sets `round-limit` and says the goal was not met rather than pretending. A judge that
fails to answer sets `blocked`, stops the loop and **keeps** the goal. Terminal
outcomes are recorded on the goal so the loop does not re-ask the same question every
turn. Goals are set and cleared by humans only — the model has no tool for it, so
"the model assigning itself work" is not forbidden here, it is impossible.

### Long commands can run in the background

`run_command` gained `background=true`: the command is spawned, and a bounded
64 KiB ring collects its output while the turn moves on. `job_output` reads a slice
(cursor-based, `wait=true` bounded at 20s and honest when it times out), `job_list`
shows this session's jobs and `job_kill` stops one. Ownership is a fence, not
secrecy: only the session that started a job can read or stop it. The gate is the
same one the foreground uses — sandbox, offline, allowlist and approval all apply,
because "put it in the background and let it run" is the natural way around an
approval prompt. A finished job's tail lands in `<home>/outbox/jobs/`.

### A todo list the model maintains itself

`todo_write` keeps a per-session list (whole-table replace, monotonic revision, 20
items, 4000 characters each, one `in_progress` at a time) and it is **injected into
the system prompt every turn** rather than kept in history — a list that survives
compaction is worth more than a list that survives nothing. Reading is the same tool
with `todos` omitted. Subagents do not get it: the list belongs to the parent's task
line.

### Per-turn tool-result budget

History had a sliding window and a token budget; the turn itself did not, and MCP tools
can return anything. `JOY_TOOL_RESULT_TOTAL_CHARS` / `JOY_TOOL_RESULT_MAX_CHARS` now
replace the largest results with stubs — full text spilled, head/tail in complete lines,
tools that budget themselves skipped, idempotent, and a failed spill never turns a
successful call into an error. The defaults are deliberately large (200k / 30k).

### Subagent reports are transcriptions, and can be structured

A report carries a header saying it is the subagent's own account, lines imitating a
role prefix or our own control markers get a backslash, and the model is told how many
were defused. It does not judge maliciousness and does not touch permission checks —
the gates do that. `delegate_task` also takes an optional `result_schema`: the answer
must be JSON matching it, one retry carries the validation error back to the child, and
a second failure falls back to prose with the reason attached.

### Lifecycle hooks (`JOY_HOOKS`)

Twelve events (`PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionRequest`,
`SessionStart`, `SessionEnd`, `Stop`, `StopFailure`, `SubagentStart`, `SubagentStop`,
`PreCompact`, `PostCompact`) can run your shell commands from `<home>/hooks.json`:
exit 2 blocks, a JSON decision can rewrite input, rewrite output or add context, and a
`PermissionRequest` hook can answer on the user's behalf. Policy events time out closed,
observation events open; a `hooks.json` changed while running is refused with a reason;
a blocking `Stop` re-runs the turn once. Shell handlers only, Unix-first — both
deliberate, both in limitations.

### Memories stop piling up

`facts` now carries a unique index on `(subject, lower(trim(content)))`, and
`Facts::add` returns `(row, was_new)`: a repeat hands back the existing row instead of
storing a second copy. Consolidation counts only genuinely new facts, and `save_note`
says "already remembered" rather than pretending to store it again. Migration 0008
collapses the duplicates that already existed before creating the index — a unique
index that cannot be created would break every later write.

### MCP: a broken server stops costing a timeout per turn

A connection carries a breaker (3 consecutive failures → 60s, isolated per server):
while it is open, calls return an explanation instead of knocking, and after the
cooldown one probe is allowed — a success clears the count. Tool names that collapse
into each other (`a`+`b_c` vs `a_b`+`c`) are de-duplicated with a suffix, and the rename
is printed because that name is what the model and the user see.
`ToolRegistry::register` refuses to replace an existing name instead of silently
swapping an implementation.

### The estimator is calibrated by measurement

`usage.input_tokens` was recorded but never fed back. The loop now returns the estimate
and the measurement for the *same* request, the app-server pairs them in
`session_context` (migration 0007), and the next turn's budget is corrected by their
ratio (clamped to 0.5–2.0, so one odd request cannot drag it off).

## 0.6.0 — Guardrails, budgets, and an offline sandbox

### Configuration is validated at startup

Out-of-range values used to silently become defaults. `Settings::validate` now
runs once after the environment and `settings.json` are merged and aborts with
the variable named. One bounds table (`joyczl-config::BOUNDS`) serves both the
startup path and `config/write`.

### Tool arguments are checked against their schema

Every tool already declared an `input_schema`; now it is compiled at
registration and enforced before the handler runs, so a bad call comes back as
`Error: 参数不符合 … 的 schema —— /subject：42 is not of type "string"` instead
of a hand-written message, and no half-applied side effect is left behind.

### A stall guard

A model looping on the same call (three identical calls in a row) or
alternating between a small set (`A,B,A,B`) now gets told, and byte-identical
long results are replaced with a reference stub. Interleaved loops need their
own test — they reset the consecutive counter — so both shapes are detected.
`TurnMeta.guard_hits` makes a stuck turn visible in traces and the dashboard.

### Context is budgeted in tokens

`ProviderInfo.context_window` plus tiktoken estimation decide when to compact:
turns are now a ceiling, tokens are the gate. A provider reporting a context
overflow is recognised (`ProviderError::ContextOverflow`) and the turn is
compacted and retried **once**. `JOY_CONTEXT_WINDOW` / `JOY_COMPACT_THRESHOLD`
tune it; validation rejects `JOY_MAX_TOKENS >= JOY_CONTEXT_WINDOW`.

### Rate limits and transient failures are retried, visibly

429/5xx/network hiccups are retried with exponential backoff (500ms up to 8s per
wait, 30s total, `JOY_LLM_RETRIES` default 2, `0` disables). Every retry emits a
`Retry` notification and `TurnMeta.retries`, so the wait is explained rather
than mysterious; the REPL prints a line and the dashboard shows a chip. A
provider that reports `Retry-After` is obeyed, within the caps. There is no
provider failover — that is a separate decision.

### Skills gain policy fields, memories gain kinds

A skill can now say `allow-model-invocation: false` ("do not summon me by
accident") and `dependencies: a, b`. Opted-out and dependency-broken skills do
not fire from keyword overlap; `$skill-name` in a message forces the body in
(and is stripped from what the model sees). A reference to something that does
not exist is a hint, not an error.

Facts carry a `kind` (`user` / `feedback` / `project` / `reference` / `fact`),
chosen by the summarizer and normalised on write, exposed through
`memory/search`, `memory/list` and the protocol's `Fact`. Failed consolidation
now backs off exponentially (1 min doubling to 1 hour) instead of retrying the
same broken rows every turn.

### Interactive approval (`JOY_APPROVAL=on-request`)

A command that the allowlist does not match can now be asked about instead of
flatly refused. The turn emits `approvalRequested` (tool, the command, why,
the deadline) and waits; `approval/respond` answers it; the REPL and the
dashboard both implement the answering side. Silence is a refusal: timeouts, a
late reply, a closed connection and having no UI at all all end in no run.
Only the allowlist gate is negotiable — the hard deny list and the sandbox
cannot be approved away. "Remember" writes the exact command into
`settings.json` and takes effect next start.

### Subagents (`JOY_DELEGATE`)

`delegate_task` hands one self-contained job to a subagent and brings back only
the conclusion, in a fresh context (empty history, no retrieval, no skills). It
gets the parent's tools **minus `delegate_task`**, so delegating again is not
refused but impossible; it cannot ask you anything; and its answers are capped
at 5 iterations (hard cap 10) and 2048 tokens. The conclusion is annotated with
the tools it used, failed calls included, so the parent can tell whether the
answer rests on something that failed. Off by default.

### Long command output is kept, not just truncated

Output past 8000 characters is still cut from what the model sees (context has
to be protected), but the full text is written to `<home>/spill/<date>/…` and
the path is reported, so it can be read back. `spill/` keeps seven days, pruned
at startup; a failed write falls back to plain truncation.

### Behaviour change: sandboxed commands are offline

`run_command` now runs with **no network** by default — seatbelt gets
`(deny network*)`, bubblewrap gets `--unshare-net`. Commands that need to
download something (a `cargo test` that fetches crates, say) must set
`JOY_EXEC_NETWORK=1`. Extra writable directories can be opened with
`JOY_EXEC_WRITABLE_ROOTS` (colon separated, each must be an existing absolute
directory) — a build cache is the typical case. The startup line reports both.

## 0.5.0 — Local, sandboxed, scheduled

- Local inference: an `ollama` provider — no key, no network, nothing leaves
  the machine. LM Studio / vLLM work through `JOY_BASE_URL` + `JOY_MODEL`
- `joy mcp serve`: Joy becomes an MCP server exposing five memory tools, so
  other agents on this machine share the same facts (memory only — nothing
  that lets another agent make Joy act)
- Sandboxed execution: `run_command` behind `JOY_EXEC`, off by default, gated
  three times — an unconfigurable hard deny list, an allowlist (empty = deny
  everything), and macOS `sandbox-exec` / Linux `bubblewrap` with writes
  confined to the working directory, the Joy home and temp. No usable sandbox
  means the command is refused, never run unsandboxed
- Context compaction: turns pushed out of the working-memory window fold into
  a rolling per-session summary (stored in state.db, never recomputed from
  scratch); a dead summarizer falls back to a deterministic excerpt
- `joy schedule`: declarative jobs from skill frontmatter (`schedule: 0 8 * * 1-5`)
  or `schedules.json`, five-field cron, one firing per minute, results written
  to the outbox
- Hybrid retrieval (`JOY_EMBEDDINGS`, keyword and vector legs fused by rank /
  RRF) + `joy memory reindex` for facts written before the switch was on
- Consolidation drops temporary statements instead of filing them as facts
- `joy skill update`: index-driven upgrades — validate, stage, back up, swap
  atomically, never downgrade
- Distribution: Dockerfile (multi-stage, non-root, bubblewrap installed) and a
  Homebrew formula template
- `docs/limitations.md`: every known boundary — deliberate or unfinished — with
  the file to open next

## 0.4.0 — Feature-complete surface

- Protocol: `turn/interrupt` (cancellation tokens + racing shutdown),
  `config/write` (settings.json persistence + hot reload), `model/list` all
  implemented; `ToolStarted` notification; `TurnMeta.usage` and
  `meta.interrupted` persisted
- CLI: terminal REPL (bare `joy`), `joy gather` morning brief,
  `joy mcp login`, `joy skill export/install/list`
- Tools: `search_web`, `create_event`/`list_events` (idempotent + ICS +
  optional Apple Calendar sync), `send_message` (outbox drafts),
  `manage_memory`, `create_skill`
- Memory: procedural Skills (progressive disclosure + keyword triggering),
  per-turn MEMORY.md mirror
- Observability: traces in `traces/<date>.jsonl`, usage ledger
  `usage.jsonl`
- Graph: the gather morning-brief workflow (four parallel scans, propose
  only, fail-open)
- MCP: browser OAuth (discovery / dynamic registration / PKCE / callback /
  token storage / refresh) alongside stdio + HTTP transports
- Evaluation: `joyczl-eval` — deterministic evals (13 scenarios), release
  gate (`just check` tail), judge
- Docs: architecture / configuration / protocol / testing / operations /
  skills / CONTRIBUTING / SECURITY (English + `.zh.md` twins)

## 0.3.0 — Dashboard and gateways

- `joyczl-ops`: axum + SSE dashboard backend; `@joy/dashboard` frontend
- Gateways: Telegram, Discord (WebSocket + heartbeat/reconnect), WeChat
  (three crypto modes + 4-second race + customer-service messages), Lark
  (long connection + pbbp2 frames + fragment reassembly)
- Sessions: `session/*` protocol methods and history paging

## 0.2.0 — Loop and orchestration

- `joyczl-provider`: 11 providers, Anthropic/OpenAI wire formats, SSE
  streaming
- `joyczl-loop`: observe → reason → act → repeat; tool errors become text
- `joyczl-memory`: retrieval gate (fail-open) + consolidation
- `joyczl-tools`: built-in tool registry; `joyczl-graph`: wave DAG engine +
  triage front door; `joyczl-mcp`: stdio + HTTP transports

## 0.1.0 — Foundation

- `joyczl-protocol`: single source of truth + TS/JSON Schema/pydantic
  generation pipeline
- `joyczl-state`: SQLite + FTS5(trigram) + migrations
- `joyczl-app-server`: JSON-RPC over stdio; `joy` CLI
