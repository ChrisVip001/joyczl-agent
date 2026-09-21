# 配置参考

[English](configuration.md) | 简体中文

配置的唯一来源是环境变量：`joyczl-config` 在进程启动时读一次，之后不可变。
Joy **不读任何 `.env` 文件**——需要 dotenv 的话由启动方自行 source
（`set -a; source .env; set +a`）。运行中的配置变更走 `config/write`
（持久化到 `<home>/settings.json`，重启后仍生效）。

## 非法值在启动期就报错

`Settings::validate` 只跑一次，位置在**环境变量与 `<home>/settings.json` 合并
之后**；非法值直接终止进程，并在消息里点名是哪个变量 —— 配错不该静默变成默认值、
过一会儿才以「行为有点怪」的形式暴露出来。同一张边界表（`joyczl-config` 的
`BOUNDS`）也校验 `config/write` 的补丁，所以两个入口不会各说各话。

| 变量 | 范围 |
|---|---|
| `JOY_MAX_ITERATIONS` | 1 – 100 |
| `JOY_MAX_TOKENS` | 128 – 200000 |
| `JOY_HISTORY_TURNS` | 0 – 1000 |
| `JOY_CONTEXT_WINDOW` | 1024 – 10000000，且必须大于 `JOY_MAX_TOKENS` |
| `JOY_COMPACT_THRESHOLD` | 0.05 – 0.95 |
| `JOY_CONSOLIDATE_EVERY` | 1 – 1000 |
| `JOY_RETRIEVAL_TOP_K` | 1 – 100 |
| `JOY_LLM_TIMEOUT` | 1 – 3600 |
| `JOY_EXEC_TIMEOUT` | 1 – 3600 |

`JOY_EXEC_ALLOW` 也在校验范围内：最多 64 条、每条非空、不含换行、不超 200 字符。
**空表是合法的** —— 它表示「什么都不放行」，而那正是默认。

## 核心变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `JOY_HOME` | `./.joy` | 状态目录：state.db、SOUL.md、skills/、traces/、outbox/、mcp.json、settings.json |
| `JOY_PROVIDER` | `anthropic` | 模型厂商：anthropic / openai / deepseek / gemini / kimi / glm / minimax / xai / openrouter / opencode_zen / opencode_go / ollama |
| `JOY_API_KEY` | — | 显式指定 key，优先于厂商默认变量 |
| `JOY_BASE_URL` | 厂商默认 | 覆盖 API 端点（测试时也可指向假端点） |
| `JOY_MODEL` / `JOY_SMALL_MODEL` | 厂商默认 | 主模型 / 便宜模型（检索门与 consolidation 用） |
| `JOY_LLM_TIMEOUT` | `120` | 单次模型调用超时（秒） |

## 行为旋钮

| 变量 | 默认 | 说明 |
|---|---|---|
| `JOY_MAX_ITERATIONS` | `10` | 单轮 loop 的迭代上限（硬停护栏） |
| `JOY_MAX_TOKENS` | `8192` | 单次模型调用输出上限（给推理模型留思考余量） |
| `JOY_HISTORY_TURNS` | `12` | 工作记忆滑窗的**上限**：只把最近 N 轮塞进 prompt（更老的折进滚动摘要，不是丢掉） |
| `JOY_CONTEXT_WINDOW` | provider 默认 | 覆盖上下文窗口的估算值（本地模型窗口差异极大，表里只是常见默认） |
| `JOY_COMPACT_THRESHOLD` | `0.8` | 用到窗口的这个比例就开始压缩 —— token 才是闸门，轮数是上限 |
| `JOY_CONSOLIDATE_EVERY` | `6` | 每 N 轮新对话触发一次 consolidation |
| `JOY_RETRIEVAL_TOP_K` | `4` | 检索门放行时拉回的 facts 条数 |
| `JOY_GRAPH_WORKFLOWS` | `0` | 打开 triage 前门图（失败开放，只能更快不能更差） |
| `JOY_APPLE_CALENDAR` | `0` | `create_event` 时经 AppleScript 同步 Calendar.app |
| `JOY_SKILL_DIRS` | — | 冒号分隔的额外技能目录（`home/skills` 之外） |
| `JOY_JUDGE_MODEL` | small model | `joy judge` 的裁判模型（裁判不是选手） |
| `JOY_GH_REPO` | — | `joy gather` 的 github scan 仓库（owner/repo） |

## 混合检索

| 变量 | 默认 | 说明 |
|---|---|---|
| `JOY_EMBEDDINGS` | `0` | 把向量相似度融进记忆检索 |
| `JOY_EMBED_MODEL` | — | embedding 模型（Ollama 上例如 `nomic-embed-text`） |

默认关着：关键词（FTS5）检索不需要模型、不需要网络。开着时两路结果按**名次**
融合（RRF，k=60）而不是按分数——bm25 与余弦不是同一个量纲，硬凑等于编数据。
embedding 服务不可用会降级成纯关键词并警告，绝不会变成「什么都想不起来」。
开关打开之前写入的事实没有向量，用 `joy memory reindex` 补齐。

## 执行命令

| 变量 | 默认 | 说明 |
|---|---|---|
| `JOY_EXEC` | `0` | 开启 `run_command` 工具（关着 = 模型看不见它） |
| `JOY_EXEC_ALLOW` | — | 放行表，逗号分隔，支持末尾 `*` 通配（`cargo test,git status,ls *`）。空 = 全部拒绝 |
| `JOY_EXEC_TIMEOUT` | `30` | 单条命令的超时（秒） |

命令一律在沙箱里跑（macOS `sandbox-exec` / Linux `bubblewrap`），写权限限制在
工作目录、Joy 的 home 与临时目录；没有沙箱的机器拒绝执行任何命令。硬拒名单
（`sudo`、`mkfs`、把下载内容交给 shell……）不可配置。见
[SECURITY.zh.md](../SECURITY.zh.md)。

## 本地推理（Ollama）

`JOY_PROVIDER=ollama` 完全跑在这台机器上：不要 key、不要网络、对话不出本机。
装好 [Ollama](https://ollama.com) 之后：

```bash
ollama pull qwen3:8b     # 主模型
ollama pull qwen3:4b     # 便宜模型（检索门、consolidation）
JOY_PROVIDER=ollama joy  # 不需要任何 API key
```

Ollama 暴露的是 OpenAI 兼容端点（`http://127.0.0.1:11434/v1`），所以复用
云端 provider 同一条 wire。LM Studio 与 vLLM 形状相同——用 `JOY_BASE_URL`
指过去、`JOY_MODEL` 指到你装着的模型即可。按模型覆盖默认值：

```bash
JOY_PROVIDER=ollama JOY_MODEL=qwen3:14b JOY_SMALL_MODEL=qwen3:4b joy
```

这是隐私路径、零成本路径，也是断网（或 key 失效）时照样能用的路径。
Ollama 没起来时报的是网络错误——`ollama serve` 把它启动起来。

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
