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
| `JOY_TOOL_RESULT_TOTAL_CHARS` | 0 – 4000000 |
| `JOY_TOOL_RESULT_MAX_CHARS` | 0 – 1000000 |
| `JOY_CONSOLIDATE_EVERY` | 1 – 1000 |
| `JOY_RETRIEVAL_TOP_K` | 1 – 100 |
| `JOY_LLM_TIMEOUT` | 1 – 3600 |
| `JOY_LLM_RETRIES` | 0 – 5 |
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
| `JOY_LLM_RETRIES` | `2` | 429/5xx/网络抖动时重试几次 —— 指数退避（500ms 起、单次上限 8s、总预算 30s），**每次必发通知**，绝不换厂商；`0` 关闭 |

## 行为旋钮

| 变量 | 默认 | 说明 |
|---|---|---|
| `JOY_MAX_ITERATIONS` | `10` | 单轮 loop 的迭代上限（硬停护栏） |
| `JOY_MAX_TOKENS` | `8192` | 单次模型调用输出上限（给推理模型留思考余量） |
| `JOY_HISTORY_TURNS` | `12` | 工作记忆滑窗的**上限**：只把最近 N 轮塞进 prompt（更老的折进滚动摘要，不是丢掉） |
| `JOY_CONTEXT_WINDOW` | provider 默认 | 覆盖上下文窗口的估算值（本地模型窗口差异极大，表里只是常见默认） |
| `JOY_COMPACT_THRESHOLD` | `0.8` | 用到窗口的这个比例就开始压缩 —— token 才是闸门，轮数是上限 |
| `JOY_TOOL_RESULT_TOTAL_CHARS` | `200000` | 一轮里所有工具结果的字符总量上限；超了就从最大的开始换成桩（`0` 关闭） |
| `JOY_TOOL_RESULT_MAX_CHARS` | `30000` | 单条结果超过它才有资格被换桩（`0` 关闭） |
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

## 生命周期钩子（`JOY_HOOKS`）

`JOY_HOOKS=1` 之后，Joy 会读 `<home>/hooks.json`，在 12 个事件上跑**你自己的 shell
命令**。这是「不改循环就能加行为」的口子：审计、策略门禁、格式化、把工具调用喂给
外部系统。

```json
{ "disableAllHooks": false,
  "hooks": {
    "PreToolUse": [
      { "matcher": "run_command", "timeout": 30, "command": "path/to/hook.sh", "args": [] }
    ],
    "PostToolUse": [ { "matcher": "*", "command": "audit.sh" } ] } }
```

**事件**（名字与 Claude Code / codex 一致，抄配置不必学第二套）：`PreToolUse` /
`PostToolUse` / `PostToolUseFailure` / `PermissionRequest` / `SessionStart` /
`SessionEnd` / `Stop` / `StopFailure` / `SubagentStart` / `SubagentStop` /
`PreCompact` / `PostCompact`。

**命令收到什么**：一段 JSON 在 stdin 上（`hook_event_name`、`session_id`、`cwd`、
`tool_name`、`tool_input`、`tool_output`、`matcher`…）。

**怎么表态**：

| 退出码 / 输出 | 含义 |
|---|---|
| `exit 0` | 放行；stdout 若是 JSON 就按下面的字段处理 |
| `exit 2` | **阻断**（唯一靠退出码阻断的方式），理由取 stdout 的 `reason` 或 stderr |
| 其它退出码 | **非阻塞错误**：记一行 stderr，动作照常 —— 钩子写坏了不该让所有工具瘫痪 |
| `{"decision":"block","reason":"…"}` | 同上，走 JSON |
| `{"updatedInput":{…}}` | 改写工具入参（改写后会**重新校验 schema**，写坏了会明说是钩子改坏的） |
| `{"updatedOutput":"…"}` | 改写工具结果 |
| `{"additionalContext":"…"}` | 给模型补一句背景（`SessionStart` 会并进这一轮的话里） |
| `{"hookSpecificOutput":{"permissionDecision":"allow\|deny"}}` | 在 `PermissionRequest` 上替用户拍板 |

**超时的两分法**：策略事件（`PreToolUse` / `PermissionRequest` / `Stop`）超时按
**阻断**处理 —— 闸门没能在时限内表态就不该默认放行；观察事件超时按**放行**处理。
单条可用 `timeout` 指定，没写就用 `JOY_HOOKS_TIMEOUT`，再没有就按事件的默认值
（交互敏感的 30 秒，其余 600 秒）。

**文件被改过就不执行**：装载时记下 `hooks.json` 的内容哈希，运行中一旦变了（别的
进程写的），新的内容**不会执行**，并在 stderr 说明。改掉一个正在生效的策略钩是
「悄悄换了闸门」那种事，宁可停下来让人看见。

`Stop` 的阻断会让这一轮**再跑一次**（上限 1 次）：它因此是目标循环（`goal/set`）的
前身，但不像目标循环那样带轮次上限、判断器与人类授权边界。

## 轮内工具结果预算

历史有滑窗和 token 预算，但**轮内**没有 —— `run_command` 自己会截到 8000 字符、
`search_web` 自己截到 400，而 MCP 工具的输出没人管。一轮里十次 MCP 调用各回 5 万
字符，就是 50 万字符进请求。

打开之后（默认就是打开的，阈值很大），一轮里工具结果的**字符总量**超过
`JOY_TOOL_RESULT_TOTAL_CHARS` 时，从最大的那条开始换成一个「桩」：完整原文写进
`<home>/spill/<日期>/`，上下文里只留**头尾各一半的完整行**和一句说明
（`…（结果共 N 字符，已截断，省略了 M 行；完整输出在 spill/…）`）。

几条规矩：

* **只动够大的**：单条没超过 `JOY_TOOL_RESULT_MAX_CHARS` 的结果不换 —— 一百条中等
  结果撑爆预算时，给每条都建一个文件比省下的上下文更贵。那种情况只在 stderr 记
  一行，不改内容。
* **绝不切半行**：半个 JSON、半条日志比少一行更难读，模型还会以为那就是全部。
  首行本身就超预算时如实说明，不硬塞。
* **落盘失败照样成立**：桩里不写路径（「完整输出在 …」而文件不存在，比不写更坏），
  更不会把一次成功的调用变成错误。
* **自预算的工具不碰**：`run_command` 自己就落盘了。
* 换桩是**幂等**的：每轮请求前都过一遍，已经是桩的直接跳过。

## 执行命令

| 变量 | 默认 | 说明 |
|---|---|---|
| `JOY_HOOKS` | `0` | 读 `<home>/hooks.json` 并在 12 个生命周期事件上跑你的 shell 命令（见下） |
| `JOY_HOOKS_TIMEOUT` | `30` | 钩子的默认超时（秒）；条目里的 `timeout` 优先 |
| `JOY_EXEC` | `0` | 开启 `run_command` 工具（关着 = 模型看不见它） |
| `JOY_DELEGATE` | `0` | 开启 `delegate_task` 工具（子代理有自己的上下文，且不能再派生） |
| `JOY_EXEC_ALLOW` | — | 放行表，逗号分隔，支持末尾 `*` 通配（`cargo test,git status,ls *`）。空 = 全部拒绝 |
| `JOY_EXEC_TIMEOUT` | `30` | 单条命令的超时（秒） |
| `JOY_APPROVAL` | `never` | 放行表没匹配上时怎么办：`never` = 拒绝，`on-request` = 问一句（没人答 = 拒绝）。对硬拒名单与沙箱无效 |
| `JOY_APPROVAL_TIMEOUT` | `120` | 等回答的秒数，超时按拒绝算 |
| `JOY_EXEC_NETWORK` | `0` | 让沙箱内的命令能联网。**默认关** —— 一条被放行的命令不该有能力把数据送出去 |
| `JOY_EXEC_WRITABLE_ROOTS` | — | 额外可写的目录（冒号分隔，必须是已存在的绝对路径），典型用途是构建缓存 |

命令一律在沙箱里跑（macOS `sandbox-exec` / Linux `bubblewrap`），写权限限制在
工作目录、Joy 的 home 与临时目录，并且**默认断网**（`JOY_EXEC_NETWORK=1`
才联网）；没有沙箱的机器拒绝执行任何命令。硬拒名单
（`sudo`、`mkfs`、把下载内容交给 shell……）不可配置。见
[SECURITY.zh.md](../SECURITY.zh.md)。

## 交互式批准一条命令

默认情况下，没匹配上放行表的命令直接拒绝。`JOY_APPROVAL=on-request` 把它变成
一个问题：这一轮发 `approvalRequested`（工具、命令原文、为什么没放行、时限）
然后**等**。终端打 `y`/`a`/其他，驾驶舱渲染一条确认小条；两边都发
`approval/respond`。沉默就是拒绝 —— 超时、答得太晚、没有界面，结局一样。

`remember: true`（终端里的 `a`，或驾驶舱的「允许并记住」）会把**被批准的那条
命令本身**（不加通配）追加进 `settings.json` 的放行表，**下次启动**生效。

## 把活交给子代理

`JOY_DELEGATE=1` 会多出 `delegate_task`：把一件自成一体的活交给子代理，只把结论
带回来 —— 适合「这件事会往对话里灌一堆检索与阅读」的场合，或者你想让它在一个
干净上下文里做。子代理拿到的是：

* 空历史 + 一份简短 system（人格 + 一句「只干这一件事」），不带检索、不带技能、
  不带摘要；
* 父轮工具表**去掉 `delegate_task`** 的副本 —— 再派一层不是「被拒绝」，而是根本
  不存在这个选项；
* 自己的迭代上限（默认 5，硬上限 10）与最多 2048 输出 token；
* 没有交互批准通道，也没有向你提问的途径。

结论回来时带着它用过的工具，**失败的调用会被标出来** —— 父轮因此能判断这份结论
是不是建立在一次失败之上。子代理的对话不留档。它与父轮同进程、同一份状态，除了
上下文之外没有别的隔离。

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
