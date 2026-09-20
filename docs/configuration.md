# Configuration reference

English | [简体中文](configuration.zh.md)

The only source of configuration is environment variables: `joyczl-config`
reads them once at process start and never again. Joy **does not read any
`.env` file** — source one yourself before launching
(`set -a; source .env; set +a`). Runtime changes go through `config/write`
(persisted to `<home>/settings.json`, surviving restarts).

## Core variables

| Variable | Default | Purpose |
|---|---|---|
| `JOY_HOME` | `./.joy` | state directory: state.db, SOUL.md, skills/, traces/, outbox/, mcp.json, settings.json |
| `JOY_PROVIDER` | `anthropic` | provider: anthropic / openai / deepseek / gemini / kimi / glm / minimax / xai / openrouter / opencode_zen / opencode_go |
| `JOY_API_KEY` | — | explicit key, takes precedence over the provider's default variable |
| `JOY_BASE_URL` | provider default | override the API endpoint (tests can point it at a fake server) |
| `JOY_MODEL` / `JOY_SMALL_MODEL` | provider default | main model / cheap model (retrieval gate and consolidation) |
| `JOY_LLM_TIMEOUT` | `120` | per-call model timeout in seconds |

## Behavior knobs

| Variable | Default | Purpose |
|---|---|---|
| `JOY_MAX_ITERATIONS` | `10` | hard iteration cap for one loop |
| `JOY_MAX_TOKENS` | `8192` | per-call output cap (headroom for reasoning models) |
| `JOY_HISTORY_TURNS` | `12` | working-memory window: only the last N turns enter the prompt |
| `JOY_CONSOLIDATE_EVERY` | `6` | run consolidation every N new turns |
| `JOY_RETRIEVAL_TOP_K` | `4` | facts fetched when the gate opens |
| `JOY_GRAPH_WORKFLOWS` | `0` | enable the triage front-door graph (fail-open, costs time only) |
| `JOY_APPLE_CALENDAR` | `0` | sync `create_event` to Calendar.app via AppleScript |
| `JOY_SKILL_DIRS` | — | colon-separated extra skill directories (besides `home/skills`) |
| `JOY_JUDGE_MODEL` | small model | referee model for `joy judge` (the referee is not a contestant) |
| `JOY_GH_REPO` | — | repository for gather's GitHub scan (owner/repo) |

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
