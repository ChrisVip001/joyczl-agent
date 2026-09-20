# 配置参考

[English](configuration.md) | 简体中文

配置的唯一来源是环境变量：`joyczl-config` 在进程启动时读一次，之后不可变。
Joy **不读任何 `.env` 文件**——需要 dotenv 的话由启动方自行 source
（`set -a; source .env; set +a`）。运行中的配置变更走 `config/write`
（持久化到 `<home>/settings.json`，重启后仍生效）。

## 核心变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `JOY_HOME` | `./.joy` | 状态目录：state.db、SOUL.md、skills/、traces/、outbox/、mcp.json、settings.json |
| `JOY_PROVIDER` | `anthropic` | 模型厂商：anthropic / openai / deepseek / gemini / kimi / glm / minimax / xai / openrouter / opencode_zen / opencode_go |
| `JOY_API_KEY` | — | 显式指定 key，优先于厂商默认变量 |
| `JOY_BASE_URL` | 厂商默认 | 覆盖 API 端点（测试时也可指向假端点） |
| `JOY_MODEL` / `JOY_SMALL_MODEL` | 厂商默认 | 主模型 / 便宜模型（检索门与 consolidation 用） |
| `JOY_LLM_TIMEOUT` | `120` | 单次模型调用超时（秒） |

## 行为旋钮

| 变量 | 默认 | 说明 |
|---|---|---|
| `JOY_MAX_ITERATIONS` | `10` | 单轮 loop 的迭代上限（硬停护栏） |
| `JOY_MAX_TOKENS` | `8192` | 单次模型调用输出上限（给推理模型留思考余量） |
| `JOY_HISTORY_TURNS` | `12` | 工作记忆滑窗：只把最近 N 轮塞进 prompt |
| `JOY_CONSOLIDATE_EVERY` | `6` | 每 N 轮新对话触发一次 consolidation |
| `JOY_RETRIEVAL_TOP_K` | `4` | 检索门放行时拉回的 facts 条数 |
| `JOY_GRAPH_WORKFLOWS` | `0` | 打开 triage 前门图（失败开放，只能更快不能更差） |
| `JOY_APPLE_CALENDAR` | `0` | `create_event` 时经 AppleScript 同步 Calendar.app |
| `JOY_SKILL_DIRS` | — | 冒号分隔的额外技能目录（`home/skills` 之外） |
| `JOY_JUDGE_MODEL` | small model | `joy judge` 的裁判模型（裁判不是选手） |
| `JOY_GH_REPO` | — | `joy gather` 的 github scan 仓库（owner/repo） |

## 各厂商的 key 变量

不设 `JOY_API_KEY` 时按 `JOY_PROVIDER` 读取对应变量：
`ANTHROPIC_API_KEY`、`OPENAI_API_KEY`、`DEEPSEEK_API_KEY`、
`GEMINI_API_KEY`、`MOONSHOT_API_KEY`（kimi）、`ZHIPU_API_KEY`（glm）、
`MINIMAX_API_KEY`、`XAI_API_KEY`、`OPENROUTER_API_KEY`、
`OPENCODE_ZEN_API_KEY`、`OPENCODE_GO_API_KEY`。

缺失时报 `PROVIDER_ERROR (-32000)`，错误信息包含领 key 的地址与应写入的
变量名。

## 搜索与评测

| 变量 | 说明 |
|---|---|
| `TAVILY_API_KEY` / `JOY_SEARCH_API_KEY` | `search_web` 升级为 Tavily（缺省用 DuckDuckGo HTML） |
| `JOY_JUDGE_MODEL` | judge 裁判模型覆盖 |
| `JOY_GH_REPO` | gather 的 github scan |

## 驾驶舱与二进制

| 变量 | 默认 | 说明 |
|---|---|---|
| `JOY_PORT` | `7777` | dashboard 端口；占用时自动向后找 10 个 |
| `JOY_DASHBOARD_DIR` | 仓库内 dist | 前端构建产物位置 |
| `JOY_BIN` | — | Python SDK / TS 客户端寻找 `joy` 二进制的显式路径 |

## 聊天网关（joy-ts）

| 平台 | 变量 |
|---|---|
| Telegram | `TELEGRAM_BOT_TOKEN`、`TELEGRAM_ALLOW`（白名单，不设即任何人可用） |
| Discord | `DISCORD_BOT_TOKEN`、`DISCORD_ALLOW`（需开启 Message Content Intent） |
| 微信 | `WECHAT_TOKEN`、`WECHAT_APP_ID`、`WECHAT_APP_SECRET`、`WECHAT_AES_KEY`（需公网 80/443） |
| 飞书 | `LARK_APP_ID`、`LARK_APP_SECRET`、`LARK_ALLOW`（open_id）、`LARK_DOMAIN=lark` |

## MCP 外挂（`<home>/mcp.json`）

```json
{"servers": [
  {"name": "fs", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]},
  {"name": "notes", "url": "https://host/mcp", "auth_env": "NOTES_API_KEY"},
  {"name": "cloud", "url": "https://host/mcp", "oauth": true}
]}
```

`command` 走 stdio、`url` 走 Streamable HTTP。`auth_env` 放环境变量**名字**
（值作 Bearer）；`oauth: true` 走浏览器授权（`joy mcp login <名>`），token
落 `mcp-auth/<名>.json`（0600）。连不上的服务器跳过并警告，Joy 照常启动。

## `settings.json`（config/write 的持久化）

dashboard 或协议的 `config/write` 把补丁累计写入 `<home>/settings.json`，
启动时叠加在环境变量之上。清除模型覆盖：把字段写成空串后提交。校验失败
的补丁整体拒绝——不合法的值不会进文件。
