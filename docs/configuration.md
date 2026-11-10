# Configuration reference

English | [简体中文](configuration.zh.md)

The only source of configuration is environment variables: `joyczl-config`
reads them once at process start and never again. Joy **does not read any
`.env` file** — source one yourself before launching
(`set -a; source .env; set +a`). Runtime changes go through `config/write`
(persisted to `<home>/settings.json`, surviving restarts).

## Model profiles and their keys (`<home>/models.json`)

Twelve providers are **pre-adapted in code** (`PROVIDERS` in `joyczl-provider`);
all a user does is pick one and supply a key:

```
/model                       list configured profiles (★ marks the active one)
/model add ds deepseek deepseek-chat --env DEEPSEEK_API_KEY
/model use ds                switch (no need to retype the key)
```

The dashboard no longer has a model panel (that is configuration, not a goal);
the entry points are the REPL's `/model` and `<home>/models.json`, over the same
`model/*` protocol methods. Keys go to the OS keychain first, falling back to
`<home>/secrets.json` (0600); `--env VAR` reads the environment and stores
nothing. `model/save` reports **where the key actually landed** (`keychain` /
`file` / `env:VAR`). Switching takes effect immediately: it rewrites Settings and
re-resolves the provider — no restart.

**Can this model see images? Declare it when you add the profile**, instead of
being told by an error later:

```
/model add ds deepseek deepseek-v4.1-flash --env DEEPSEEK_API_KEY --vision
```

- no flag: follow the vendor's catalog row (the vendor's **default** model);
- `--vision`: this model sees images even though the vendor's default does not;
- `--no-vision`: this model is text-only even though the vendor's default sees.

When one vendor ships both kinds (`deepseek-v4.1-flash` beside
`deepseek-v4-pro`), the profile is the only layer that can be right. The file
form is `"vision": true` on that profile in `models.json`; to change the
**whole vendor**, edit `models-catalog.json` instead (see "The model catalog").

**Only a human can change any of this**: the model has no `model/*` tool and
never sees a key.

## Model routing (`<home>/model-routes.json`)

Instead of picking the model yourself, let the server pick it per turn —
a tier abstraction ("You don't pick the model, the task does"):

```json
{
  "mode": "auto",
  "routes": { "reasoning": "ds-r1" }
}
```

- `mode: "fixed"` (the default — a missing or broken file also means fixed)
  keeps the active profile for every turn; routing never intervenes.
- `mode: "auto"` picks a **tier** per turn, then maps tier → profile id:
  token estimate over half the chat profile's context window → `reasoning`;
  otherwise `chat`. Messages in the ambiguous band (≥2k tokens, under half
  the window) additionally ask the cheap small-model once ("is this deep
  reasoning?"); any failure or vague answer falls back to the L0 answer
  (fail-open).
- Missing tier entries fall through a default chain: `reasoning`/`fast` →
  `chat` → the active profile. A route pointing at a deleted profile falls
  back to the active profile and says so in `TurnMeta.routeReason`.
- `JOY_MODEL_ROUTE=auto|fixed` forces the mode from the environment
  (for CI / unattended hosts where the home file is out of reach).
- Editing: REPL `/route`, `/route auto|fixed`, `/route set <tier> <id>`
  (the file is re-read every turn — changes take effect immediately).
- Routing is a **server-side policy**: the model has no tool to switch its
  own model, and the human's active profile always wins.

Routing is transparent about itself: `TurnMeta.routeReason` records
`auto:reasoning → ds-r1` or `fallback: …` whenever a choice or a fallback
actually happened; the dashboard footer prints it.

## Model routing (`<home>/model-routes.json`)

Instead of picking the model yourself, let the server pick it per turn —
A tier abstraction ("You don't pick the model, the task does"):

```json
{
  "mode": "auto",
  "routes": { "reasoning": "ds-r1" }
}
```

- `mode: "fixed"` (the default — a missing or broken file also means fixed)
  keeps the active profile for every turn; routing never intervenes.
- `mode: "auto"` picks a **tier** per turn, then maps tier → profile id:
  token estimate over half the chat profile's context window → `reasoning`;
  otherwise `chat`. Messages in the ambiguous band (≥2k tokens, under half
  the window) additionally ask the cheap small-model once ("is this deep
  reasoning?"); any failure or vague answer falls back to the L0 answer
  (fail-open).
- Missing tier entries fall through a default chain: `reasoning`/`fast` →
  `chat` → the active profile. A route pointing at a deleted profile falls
  back to the active profile and says so in `TurnMeta.routeReason`.
- `JOY_MODEL_ROUTE=auto|fixed` forces the mode from the environment
  (for CI / unattended hosts where the home file is out of reach).
- Editing: REPL `/route`, `/route auto|fixed`, `/route set <tier> <id>`
  (the file is re-read every turn — changes take effect immediately).
- Routing is a **server-side policy**: the model has no tool to switch its
  own model, and the human's active profile always wins.

Routing is transparent about itself: `TurnMeta.routeReason` records
`auto:reasoning → ds-r1` or `fallback: …` whenever a choice or a fallback
actually happened; the dashboard footer prints it.

### A task can declare its tier

`task/create` and `turn/start` both accept `tier` (`fast` / `chat` / `reasoning`):

- **A declaration beats the mode**: even with `mode: "fixed"`, a task or a
  caller saying `reasoning` runs this turn on the reasoning profile — fixed is
  the default policy, not a veto;
- A task's declared tier is stored on the task row (migration 0016) and every
  round of that task reuses it — no need to repeat it per turn;
- A mistyped tier is an error on the spot, never a silent `chat` (that would put
  the bill on the wrong model);
- The dashboard's model panel gained a tier block: an auto switch plus three
  "tier ↔ profile" rows, writing the same `model-routes.json` as REPL `/route`.

## Inbound webhooks (`<home>/webhooks.json`)

Another system knocks, Joy runs one turn (CI failure, form submitted, payment
landed…). Without this file there is **no door at all** (404), and the model
never learns the hook exists:

```json
{"hooks": [{
  "name": "deploy",
  "secret": "env:JOY_HOOK_SECRET",
  "prompt": "Production is unhappy — handle this event:\n{{payload}}",
  "session": "hook:deploy",
  "tier": "reasoning",
  "signature": true,
  "limit": 6,
  "callback": "https://ci.example.com/joy/hook-result"
}]}
```

- `POST /api/hook/<name>`; the `X-Joy-Secret` header must match `secret`
  (`env:VAR` is read per request; unreadable = refuse, never degrade to an
  empty string). Wrong or missing is always 401;
- This route sits **outside** the dashboard token layer on purpose: the calling
  machine has never heard of `JOY_OPS_TOKEN`; what it proves is that it knows
  this hook's secret. In return a hook can only **hand over one message** — it
  cannot read state or start a second turn;
- Unknown name, `enabled: false`, no config file — all three are 404 (the door
  does not exist, and its existence is not revealed);
- `{{payload}}` is replaced by the raw request body; without the placeholder the
  body is appended to the prompt (silently dropping a body would be worse);
- The turn is still an ordinary turn: tools keep their approval cards, and
  nobody is at the keyboard — destructive tools time out and are denied.
  A webhook adds no capability;
- The answer is a `RunResponse` (`turnId` / `reply` / `model` / `provider` /
  `routeReason`) — the same shape as the task API (pushed to the callback in async
  mode).

### Three optional fences

- **`signature: true` — no replays.** The request must carry `X-Joy-Timestamp`
  (epoch seconds) and `X-Joy-Signature` =
  `hex(HMAC-SHA256(secret, "<timestamp>.<body>"))`. A timestamp more than ±300s
  away from our clock → 401; the same signature seen twice inside the window →
  409. The other side verifies with tools it already has:

  ```bash
  BODY='{"status":"failed"}'; TS=$(date +%s)
  SIG=$(printf '%s' "$TS.$BODY" | openssl dgst -sha256 -hmac "$JOY_HOOK_SECRET" -r | cut -d' ' -f1)
  curl -X POST "http://127.0.0.1:7777/api/hook/deploy" \
    -H "x-joy-timestamp: $TS" -H "x-joy-signature: $SIG" -d "$BODY"
  ```

  **Retries must be re-signed** (a new timestamp): each signature is accepted once.
  Without `signature`, plain `X-Joy-Secret` still works — it proves "knows the
  secret" and **cannot refuse a replay** (the same request runs a second turn).
  That difference is why the signature exists.
- **`limit: N` — rate.** At most N accepted requests per rolling minute per hook;
  only requests that pass auth are counted (guessing secrets does not burn the
  budget), and going over answers 429 + `Retry-After` seconds. The counter is
  per-process: a restart clears it, multiple processes do not share it.
  `limit: 0` is refused at load ("configured but can never pass" is a typo, not
  a policy).
- **`callback: "https://…"` — async.** The caller gets `202` +
  `{"accepted":true,"hook":…,"session":…}` at once; the turn runs in the
  background and its result is POSTed to that URL:
  `{"hook","session","ok":true,"result":RunResponse}` or
  `{"hook","session","ok":false,"error":{code,message}}`. Three attempts (1s and
  2s apart), **no queue**: if all three fail (or the process dies first) that
  delivery is lost — the answer itself is in the session history and is not gone.
  A callback must be an http(s) URL, or loading fails loudly.

## Task API (`POST /api/run`)

"One turn in, one JSON answer out" for programs: the body is `TurnStartParams`
(the same shape as `/api/turn`), no streaming, and it returns when the turn ends:

```bash
curl -sS -X POST http://127.0.0.1:7777/api/run \
  -H "Authorization: Bearer $JOY_OPS_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"message":"Summarise yesterday in three points","tier":"reasoning"}'
```

- The dashboard keeps SSE (a person wants to watch the words arrive); this
  endpoint is for CI / cron / another agent;
- It takes the same Bearer: it is part of the dashboard, just without a page;
- `model` and `routeReason` are the visible proof that the tier pin took effect.

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
| `JOY_PROVIDER` | `anthropic` | **only** the fallback provider when no profile is configured (local models start zero-config this way). Models and keys live in `<home>/models.json` |
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

With `JOY_HOOKS=1`, Joy reads `<home>/hooks.json` and runs **your handlers** on
twelve events — the place to add behaviour without editing the loop: auditing, policy
gates, formatting, feeding tool calls to something else.

```json
{ "disableAllHooks": false,
  "hooks": {
    "PreToolUse": [
      { "matcher": "run_command", "timeout": 30, "command": "path/to/hook.sh", "args": [] }
    ],
    "PostToolUse": [ { "matcher": "*", "command": "audit.sh" } ],
    "Stop": [ { "url": "http://127.0.0.1:9000/joy-policy" } ] } }
```

Each entry names **exactly one handler**: `command` (shell) or `url` (HTTP) —
both, or neither, is a config error and the entry is skipped with a stderr line.

**Events** (named as commonly used in the ecosystem): `PreToolUse`, `PostToolUse`,
`PostToolUseFailure`, `PermissionRequest`, `SessionStart`, `SessionEnd`, `Stop`,
`StopFailure`, `SubagentStart`, `SubagentStop`, `PreCompact`, `PostCompact`.

**What the handler gets**: a JSON payload (`hook_event_name`, `session_id`,
`cwd`, `tool_name`, `tool_input`, `tool_output`, `matcher`, …) — on stdin for a
shell command, as the POST body for a URL.

**How it answers**:

| Exit / output | Meaning |
|---|---|
| `exit 0` | allow; a JSON stdout is parsed as below |
| `exit 2` | **block** (the only exit code that blocks), reason from `reason` or stderr |
| any other code | a non-blocking error: one stderr line, the action proceeds — a broken hook must not disable every tool |
| HTTP `200` | allow; a JSON response body is parsed as below |
| HTTP anything else / connection refused | "could not answer", not "no": one stderr line, the action proceeds (timeouts keep the policy/observation split below) |
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
| `JOY_WRITE` | `0` (`1` in the dashboard) | enable the `write_file` / `edit` tools (off = the model never sees them). Writable boundary: **only** this turn's chosen working directory plus `JOY_EXEC_WRITABLE_ROOTS`; with no folder chosen nothing can be written |
| `JOY_DELEGATE` | `0` | enable the `delegate_task` tool (a subagent gets its own context; it cannot delegate again) |
| `JOY_EXEC_ALLOW` | — | allowlist, comma separated, trailing `*` wildcards (`cargo test,git status,ls *`). Empty = deny everything |
| `JOY_EXEC_TIMEOUT` | `30` | per-command timeout in seconds |
| `JOY_APPROVAL` | `never` (`on-request` in the dashboard) | what to do when the allowlist does not match: `never` = refuse, `on-request` = ask (no answer = refusal). Never applies to the deny list or the sandbox |
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

## Writing files (`JOY_WRITE`)

`JOY_WRITE=1` registers two tools: `write_file` (whole-file write, overwrites)
and `edit` (one exact-match replacement — `old_string` must appear exactly
once, 0 or several matches are refused, and the reply carries a change hunk).
The CLI and hub keep the conservative off-by-default; the **single-user dashboard
defaults to on**, because a human is watching and can approve (explicit
`JOY_WRITE` / `JOY_APPROVAL` environment variables always win).

In the dashboard the working directory is a folder-picker button, not a text
field: the chosen folder is the **only** writable root (plus
`JOY_EXEC_WRITABLE_ROOTS`), and with no folder chosen nothing is writable. The
Joy home and the process cwd are deliberately outside it — an assistant with a
workspace must not wander into memory or wherever the server was started.
Outside the boundary the write is refused before approval is ever consulted.
Content must be UTF-8 text (no NUL bytes) and at most 1 MiB. The dashboard also
defaults `JOY_APPROVAL=on-request`: **every** write shows an approval card in the
chat window (allow / allow and remember / deny; no answer times out as denial).
The config panels were removed from the page — approvals stay where a human can
answer them.

## The model catalog (`models-catalog.json`)

Twelve providers ship in the binary (`joyczl-provider`). To add one — or to
retune a built-in's defaults — without touching code, write
`<home>/models-catalog.json`: a JSON array, each row either **patches an
existing id** (any subset of fields) or **adds a new provider** (`wire`,
`model` required; `key_env` empty or absent = keyless local endpoint;
`context_window` defaults to 32768). Example:

```json
[
  { "id": "deepseek", "context_window": 999999 },
  { "id": "mybox", "wire": "openai", "model": "qwen3:8b",
    "base_url": "http://127.0.0.1:11434/v1" }
]
```

The file is read **once at boot** and merged into the catalog; `/model`, the
dashboard's Model panel and `model/list` all see the merged result, and
`model/save` validates against it. Unknown fields are rejected (a typo that
silently does nothing is worse than an error); a broken file or row costs one
stderr line and keeps the built-in table — it never blocks boot. Restart the
process after editing. Note the embedding endpoint (hybrid retrieval) still
selects from the built-in table only.

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
the same shape — add a profile pointing at them (`--url` for the endpoint)
you have loaded. Override the defaults per model:

```bash
JOY_PROVIDER=ollama joy          # or: /model add local ollama qwen3:14b
```

This is the privacy path and the zero-cost path, and the one that keeps
working when the network (or a key) does not. If Ollama is not running the
error surfaces as a network error — start it with `ollama serve`.

## Per-provider key variables

Joy does **not** read these implicitly — a key only comes from the profile you
configured (`--env VAR` declares "read this one"). The names are each vendor's
conventional one, matching `JOY_PROVIDER`: `ANTHROPIC_API_KEY`,
`OPENAI_API_KEY`, `DEEPSEEK_API_KEY`,
`GEMINI_API_KEY`, `MOONSHOT_API_KEY` (kimi), `ZHIPU_API_KEY` (glm),
`MINIMAX_API_KEY`, `XAI_API_KEY`, `OPENROUTER_API_KEY`,
`OPENCODE_ZEN_API_KEY`, `OPENCODE_GO_API_KEY`.

A missing key answers `PROVIDER_ERROR (-32000)` with where to get one and
which variable to set.

## Search and evaluation

| Variable | Purpose |
|---|---|
| `TAVILY_API_KEY` / `JOY_SEARCH_API_KEY` | upgrade `search_web` to Tavily (default: keyless Bing HTML) |
| `BRAVE_API_KEY` | second keyed backend in the chain (tried after Tavily) |
| `JOY_SEARCH_MAX_PER_TURN` | per-turn `search_web` cap (default 20, range 1–200) |
| `JOY_SEARCH_BING` | `0` disables the keyless Bing backend (default on; the one that works where DDG is blocked) |
| `JOY_SEARCH_DDG` | `0` disables the keyless DuckDuckGo backend (default on; last resort) |
| `JOY_JUDGE_MODEL` | judge referee override |
| `JOY_GH_REPO` | gather's GitHub scan target |

Search backends form an ordered chain: Tavily, then Brave; without any key the
keyless **Bing HTML → DuckDuckGo** pair enters the plan (Bing is usually reachable
on networks that block DDG, so DDG is only the last resort). After a keyed backend
fails, the free endpoints stay out of the plan — one gamble per turn, not two.
When no backend is usable, `search_web` is not registered at all and startup says
so; search results are wrapped as untrusted external data, same as a fetched
page body.

## Dashboard and binaries

| Variable | Default | Purpose |
|---|---|---|
| `JOY_PORT` | `7777` | dashboard port; occupied ports scan upward |
| `JOY_DASHBOARD_DIR` | in-repo dist | location of the built frontend |
| `JOY_NOTIFY` | off | when on, reads `<home>/notify.json` (`{"slack": "webhook URL"}`) and pushes notifications: **approval requests / background-job finish / goal outcomes**. Fire-and-forget — a failed send is one stderr line and the turn carries on. The URL is admin-configured, so pointing it at a self-hosted intranet endpoint (ntfy/Gotify — anything accepting `{"text": …}`) is legitimate; no SSRF guard applies
| `JOY_OPS_HOST` | `127.0.0.1` | bind address. **A non-loopback host requires `JOY_OPS_TOKEN` or startup is refused** (a wrong config should fail to boot) |
| `JOY_OPS_TOKEN` | — | Bearer token for `/api/*` (constant-time compare). Optional on loopback; when set, local requests need it too. The dashboard asks once on first 401 and stores it in localStorage. **Deliberately not in `config/read`**: deployment parameters must not be visible to the model |
| `JOY_HUB_ROOT` | — | root directory for multi-user mode (or pass `joy hub --root`) |
| `JOY_HUB_MAX_OPEN` | 32 | how many per-user app-servers hub keeps open at once (each is a child process). When full, the least-recently-used idle instance is evicted; if everything is in use it over-commits rather than failing requests |
| `JOY_HUB_IDLE_SECS` | 900 | close a user's app-server after this many idle seconds. 0 = no idle eviction, only the cap applies |
| `JOY_BIN` | — | explicit `joy` binary path for the Python SDK / TS clients |

## Multi-user on one machine (`joy hub`)

The shape for teams: **one ops process + N users + one full home each** (the same
shape across the reference
implementations; none of them build an account system).

```
JOY_HUB_ROOT=~/joy-hub joy hub           # multi-user dashboard, still binds 127.0.0.1 by default
```

- **Accounts are self-service, seats are capped**: open the dashboard and
  register (username + password + device name). `JOY_HUB_MAX_OPEN` doubles as
  the seat count — full table, registration closes. The **first** user must
  register from loopback (or via `joy hub adduser alice --root ~/joy-hub`,
  which mints a CLI token printed once) — a fresh hub on a public bind never
  lets a stranger claim it.
- **Logging in pairs the device** (登录即配对): each device name gets its own
  token, stored in `users.json` (0600) next to the PBKDF2-SHA256 password
  credential (ring, salted, constant-time verify). Same device + password
  returns the same token — logging in twice does not multiply tokens. The old
  flat `user → token` file migrates on load (token becomes device `cli`).
- **Password reset is a CLI matter** (deliberate): re-register the same
  username from loopback is refused while it exists; an owner fixes a lost
  password by removing the user's entry in users.json and re-adding —
  no email flow, no reset links.
- **Isolation is structural**: one home per user (state.db, models.json, jobs,
  skills — the full set), each with its own app-server child, spawned lazily on
  first use. A's keys cannot appear in B's paths — there is no shared path.
- User names allow lowercase letters, digits, and hyphens (≤32) — names become
  directories, so `../` is refused at load time.
- Auth semantics are **the same Bearer path** as single-user mode: a static
  `JOY_OPS_TOKEN` and per-device hub tokens run through the same code.
- **Cross-device sync**: `GET /api/events` (Bearer) streams lightweight pings
  — "a turn started / finished somewhere", "an approval is waiting". Clients
  pull `/api/data` for details; pings carry no payload. CORS is permissive on
  `/api/*` because clients (Tauri shell, PWA) authenticate by header, never
  cookies.

## Connectors (`<home>/connectors.json`)

"Draft, never send" is the default; a connector is the one exception, fenced three ways:

1. **Off until configured.** No `connectors.json` → no connector tools exist; the model
   cannot even see their names.
2. **Dry run by default.** A configured connector with `"live": false` (the default)
   only writes drafts to `<home>/outbox/`; nothing leaves the machine.
3. **Every live send asks once.** The approval card names the recipients and previews
   the body; refusal/timeout = no send. The allowlist is a hard boundary, and
   **empty means nobody**.

```json
{
  "email": {
    "smtp_host": "smtp.example.com",
    "smtp_port": 465,
    "from": "joy@example.com",
    "username": "joy@example.com",
    "secret": "keychain:connector:email",
    "allow_recipients": ["@example.com", "alice@other.org"],
    "live": false
  },
  "feishu": {
    "app_id": "cli_a1b2c3",
    "app_secret": "env:FEISHU_APP_SECRET",
    "receive_id_type": "open_id",
    "allow_recipients": ["ou_xxx", "oc_yyy"],
    "live": false
  }
}
```

| Field | Meaning |
|---|---|
| `smtp_port` | 465 = implicit TLS, 587 = STARTTLS, anything else = plaintext (local relay/tests only) |
| `secret` / `app_secret` | `env:VAR` or a `SecretStore` account (keychain first, 0600 plaintext fallback — reported honestly) |
| `allow_recipients` | email: exact address or `@domain`; feishu: exact `open_id` / `chat_id` |
| `live` | only `true` actually sends; flipping it is a human act |
| `receive_id_type` | `open_id` (a person) or `chat_id` (a group) |
| `base_url` | for self-hosted Lark; defaults to `https://open.feishu.cn` |
| `imap.*` | the inbound block: `host` / `port` / `mailbox` / `poll_secs` / `mode` (`notify` or `turn`) |
| `as` | Feishu sending identity: `app` (default) or `user` (needs `joy connector feishu login` first) |

The human's own commands (the terminal sends without an approval card — the person
typing is the approval):

```bash
joy connector list
joy connector email --to alice@corp.com --subject Weekly --body text [--dry-run]
joy connector feishu --to ou_xxx --text text [--dry-run]
```

Model-side tools are `email_send` / `feishu_send`, registered only when the connector is
configured and its allowlist is non-empty. The dashboard's connector panel is read-only:
which connector, whether it can really send, how wide the allowlist is.

### Inbound mail (`email.imap`)

Outbound is "draft → a human nods → send"; inbound is the other direction: the outside
world wants in by itself. So this does the most conservative thing available — **record
the mail as it is, and let a person decide whether to act on it**:

```json
{"email": {
  "smtp_host": "smtp.example.com", "smtp_port": 465, "from": "joy@example.com",
  "username": "joy@example.com", "secret": "keychain:connector:email",
  "allow_recipients": ["@example.com"],
  "imap": {"host": "imap.example.com", "port": 993, "mailbox": "INBOX",
           "poll_secs": 300, "mode": "notify"}
}}
```

- No `imap` block = **no inbound at all**: no poller, no inbox surface;
- `port`: 993 implicit TLS, 143 STARTTLS, anything else plaintext (local fake servers
  only); `poll_secs` has a 30-second floor; `mode` is `notify` or `turn`, and a typo
  fails startup;
- `mode: "notify"` (default) records + notifies, and the dashboard's inbox card button
  (`inbox/summarize`) is the human saying "handle these now"; `mode: "turn"` turns new
  mail into a turn by itself — a standing authorization written in the file, never the
  model's decision;
- The first boot only records the high-water mark and **does not pull history** (those
  few hundred old messages are not "new"); after that, deduping is by UID. Bodies are
  stored as decoded plain text; attachments are not stored;
- Fetching always uses `BODY.PEEK[]`: Joy reading a mailbox never marks another person's
  mail as read.

### Feishu user identity (`"as": "user"`)

By default messages come from the app identity (`tenant_access_token`). To send as a
person, run one browser authorization first:

```bash
joy connector feishu login     # authorization-code grant, token at <home>/connector-auth/feishu.json (0600)
joy connector feishu status    # valid / expired (refreshed before the next send)
joy connector feishu logout    # delete the token
```

Then set `"as": "user"` in the feishu block of `connectors.json`. The callback is fixed
at `127.0.0.1:41766/callback` and must be registered **exactly** in the app console;
there is no PKCE.

The token is **rotating**: each refresh token dies when it is used, so renewals are
serialized (in-process through `connector-auth/feishu.lock`). The long-lived process
(app-server) renews it **ten minutes before each expiry** — a send never pays a network
round trip for it, and a laptop that slept past the expiry wakes up still valid. When
Feishu refuses the grant (authorization revoked, refresh token dead) it says so once and
asks for a human login instead of writing the same line into the log every five minutes.
The CLI keeps its lazy path ("expired, will refresh on the next send" in
`joy connector feishu status`).

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
