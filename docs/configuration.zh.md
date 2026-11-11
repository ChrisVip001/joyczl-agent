# 配置参考

[English](configuration.md) | 简体中文

配置的唯一来源是环境变量：`joyczl-config` 在进程启动时读一次，之后不可变。
Joy **不读任何 `.env` 文件**——需要 dotenv 的话由启动方自行 source
（`set -a; source .env; set +a`）。运行中的配置变更走 `config/write`
（持久化到 `<home>/settings.json`，重启后仍生效）。

## 多套模型与它们的密钥（`<home>/models.json`）

12 家 provider **预适配在代码里**（`joyczl-provider` 的 PROVIDERS），用户要做的
只是选一家、给 key：

```
/model                      看已配好的套（★ 是当前）
/model add ds deepseek deepseek-chat --env DEEPSEEK_API_KEY
/model use ds              切换（不用重发密钥）
```

驾驶舱不放模型配置面板（那是配置，不是目标）；入口就是 REPL 的 `/model` 和
`<home>/models.json`，协议上仍是 `model/*` 那三个方法。密钥优先交给
系统钥匙串，兜底写 `<home>/secrets.json`（0600）；`--env 变量名` 则只读环境变量、
不存下来。`model/save` 的应答会如实说**实际落在哪**（`keychain` / `file` /
`env:VAR`）。切换立刻生效：改的是 Settings 并重解析 provider，不用重启。

**这套模型能看图片吗？加配置时顺手说清**，别等发图被拦再回头改：

```
/model add ds deepseek deepseek-v4.1-flash --env DEEPSEEK_API_KEY --vision
```

- 不写 `--vision`：跟随厂商目录行（`models-catalog.json` 里那一家的默认）；
- `--vision`：这套模型看得见图，即使厂商的默认模型是纯文本；
- `--no-vision`：这套是纯文本，即使厂商默认模型看得见图。

同一家既有看图模型也有纯文本模型时（`deepseek-v4.1-flash` 与 `deepseek-v4-pro`），
profile 才是说得准的那一层。等价的文件写法是在 `models.json` 那条里写
`"vision": true`；要让**整个厂商**默认都算，再去改 `models-catalog.json`
（见下文「模型目录」）。

**只有人能改这些**：模型没有 `model/*` 工具，也拿不到密钥。

## 模型路由（`<home>/model-routes.json`）

不自己选模型，让服务端按轮选 —— 任务档位抽象
（"You don't pick the model, the task does."）：

```json
{
  "mode": "auto",
  "routes": { "reasoning": "ds-r1" }
}
```

- `mode: "fixed"`（默认 —— 文件不存在或损坏同样是 fixed）：每轮都用
  active profile，路由完全不介入。
- `mode: "auto"`：每轮选一个**档位**，再查表映射到 profile id ——
  token 估算超过 chat 档上下文窗口的一半 → `reasoning`；否则 `chat`。
  落在模糊带（≥2k token、未过半窗口）的消息会额外问一次便宜小模型
  （「这是要动脑的活吗？」），失败或含糊一律落回 L0 结论（fail-open）。
- 档位缺项按缺省链回落：`reasoning`/`fast` → `chat` → active profile。
  指向已删除 profile 的映射回落 active，并在 `TurnMeta.routeReason`
  里如实说明。
- `JOY_MODEL_ROUTE=auto|fixed` 从环境强制指定模式（CI / 无人值守改不了
  home 文件时用）。
- 编辑入口：REPL 的 `/route`、`/route auto|fixed`、`/route set <档位> <配置名>`
  （文件每轮重读 —— 改完即生效）。
- 路由是**服务端策略**：模型没有换自己模型的工具，人类的 active profile
  永远优先。

路由对自己是透明的：真的发生了选择或回落时，`TurnMeta.routeReason`
记下 `auto:reasoning → ds-r1` 或 `fallback: …`，驾驶舱页脚照实打印。

### 档位可以由任务声明

`task/create` 与 `turn/start` 都接受 `tier`（`fast` / `chat` / `reasoning`）：

- **声明优先于模式**：即使 `mode: "fixed"`，任务或调用方说了 `reasoning`，
  这一轮就走 reasoning 档对应的 profile —— fixed 是默认策略，不是否决权；
- 任务声明的档位挂在任务行上（migration 0016），它的每一轮都沿用，
  不必每轮重说；
- 拼错的档位当场报错，不会静默当成 `chat`（那会让账单落在错误的模型上）；
- 驾驶舱「模型」面板新增档位区：自动开关 + 三行「档位 ↔ 配置」，
  与 REPL 的 `/route` 写的是同一个 `model-routes.json`。

## 入站 webhook（`<home>/webhooks.json`）

别的系统敲一下，Joy 跑一轮（CI 失败、表单提交、支付到账……）。没有这个文件
就**没有这扇门**（404），模型也完全不知道它存在：

```json
{"hooks": [{
  "name": "deploy",
  "secret": "env:JOY_HOOK_SECRET",
  "prompt": "线上出事了，按这个事件处理：\n{{payload}}",
  "session": "hook:deploy",
  "tier": "reasoning",
  "signature": true,
  "limit": 6,
  "callback": "https://ci.example.com/joy/hook-result"
}]}
```

- `POST /api/hook/<name>`，请求头 `X-Joy-Secret` 必须与 `secret` 一致
  （`env:VAR` 在请求时读取；读不到 = 拒绝，不退化成空串），错或缺失一律 401；
- 这条路由**故意在 dashboard 的 token 层之外**：敲门的机器不知道
  `JOY_OPS_TOKEN`，它只证明自己知道这个 hook 的密钥；反过来，hook 只能
  **递一句话**，读不了任何状态、开不了第二个回合；
- 未知名字、`enabled: false`、没有配置文件 —— 三种情况都是 404
  （门不存在，也不透露它存在过）；
- `{{payload}}` 换成请求体原文；没有占位符就把它接在 prompt 后面
  （静默丢掉 body 更糟）；
- 这一轮仍然是普通的一轮：工具照旧要审批卡，而键盘前没有人 ——
  破坏性工具会在超时后被拒。webhook 不新增任何能力；
- 应答是 `RunResponse`（`turnId` / `reply` / `model` / `provider` /
  `routeReason`），与任务 API 同一个形状（异步模式下它被推到 callback）。

### 三个可选栅栏

- **`signature: true` —— 防重放**。请求必须带 `X-Joy-Timestamp`（秒级整数）与
  `X-Joy-Signature` = `hex(HMAC-SHA256(secret, "<timestamp>.<body>"))`。
  时间戳与本地时钟相差超过 ±300 秒 → 401；同一个签名在窗口内第二次出现 →
  409。验证方式就是另一侧已有的工具：

  ```bash
  BODY='{"status":"failed"}'; TS=$(date +%s)
  SIG=$(printf '%s' "$TS.$BODY" | openssl dgst -sha256 -hmac "$JOY_HOOK_SECRET" -r | cut -d' ' -f1)
  curl -X POST "http://127.0.0.1:7777/api/hook/deploy" \
    -H "x-joy-timestamp: $TS" -H "x-joy-signature: $SIG" -d "$BODY"
  ```

  **重试要重新签名**（换一个新时间戳）：同一个签名只被接受一次。
  不开 `signature` 时 `X-Joy-Secret` 照旧能用 —— 但它只能证明「知道密钥」，
  **不能否认重放**（同一份请求重发就跑第二轮）。这是要开签名的理由。
- **`limit: N` —— 限流**。每个 hook 每分钟最多 N 次，计数只算**通过鉴权**的
  请求（乱试不会烧掉额度）；超了回 429 + `Retry-After` 秒数。进程内计数：
  重启清零，多进程不共享。`limit: 0` 在加载时直接报错（「配了但永不放行」
  是打错字，不是策略）。
- **`callback: "https://…"` —— 异步**。调用方立刻拿到 `202` +
  `{"accepted":true,"hook":…,"session":…}`，这一轮在后台跑完，结果 POST 到
  该 URL：`{"hook","session","ok":true,"result":RunResponse}` 或
  `{"hook","session","ok":false,"error":{code,message}}`。三次投递（间隔 1s、
  2s），**没有队列**：三次都没成（或进程中途退出）就丢了这次投递 —— 但答案
  已经在会话历史里，不会消失。callback 必须是 http(s) URL，否则加载即报错。

## 任务 API（`POST /api/run`）

程序用的「一轮进、一段 JSON 出」：请求体就是 `TurnStartParams`
（与 `/api/turn` 同一个形状），不流式，等这一轮结束才返回：

```bash
curl -sS -X POST http://127.0.0.1:7777/api/run \
  -H "Authorization: Bearer $JOY_OPS_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"message":"把昨天的日志总结成三点","tier":"reasoning"}'
```

- 驾驶舱仍走 SSE（人想看着字一个个出来），这个端点是给 CI / cron /
  另一个 agent 用的；
- 走同一条 Bearer：它属于 dashboard，只是没有页面；
- `model` 与 `routeReason` 是「档位真的生效了吗」的可见证据。

## 非法值启动即失败

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
| `JOY_GOAL_MAX_ROUNDS` | 1 – 20 |
| `JOY_EXEC_TIMEOUT` | 1 – 3600 |

`JOY_EXEC_ALLOW` 也在校验范围内：最多 64 条、每条非空、不含换行、不超 200 字符。
**空表是合法的** —— 它表示「什么都不放行」，而那正是默认。

## 核心变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `JOY_HOME` | `./.joy` | 状态目录：state.db、SOUL.md、skills/、traces/、outbox/、mcp.json、settings.json |
| `JOY_PROVIDER` | `anthropic` | 模型厂商：anthropic / openai / deepseek / gemini / kimi / glm / minimax / xai / openrouter / opencode_zen / opencode_go / ollama |
| `JOY_PROVIDER` | `anthropic` | **只**是一套都没配时的兜底厂商（本地模型靠它零配置起步）。真正的模型与密钥在 `<home>/models.json` |
| `JOY_LLM_TIMEOUT` | `120` | 单次模型调用超时（秒） |
| `JOY_LLM_RETRIES` | `2` |
| `JOY_GOAL_MAX_ROUNDS` | `5` | 一个目标最多自动续几轮。超了就置 `round-limit` 并如实说明没完成 —— 不伪装完成、也不清掉目标 | 429/5xx/网络抖动时重试几次 —— 指数退避（500ms 起、单次上限 8s、总预算 30s），**每次必发通知**，绝不换厂商；`0` 关闭 |

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
| `JOY_GOOGLE_CALENDAR` | `0` | Google Calendar 的连接流程**还没实现**；设了它 `create_event` 会在回执里明说「这次没有同步」，而不是沉默 —— 诚实开关，不是同步开关 |
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

`JOY_HOOKS=1` 之后，Joy 会读 `<home>/hooks.json`，在 12 个事件上跑**你自己的
handler**。这是「不改循环就能加行为」的口子：审计、策略门禁、格式化、把工具调用
喂给外部系统。

```json
{ "disableAllHooks": false,
  "hooks": {
    "PreToolUse": [
      { "matcher": "run_command", "timeout": 30, "command": "path/to/hook.sh", "args": [] }
    ],
    "PostToolUse": [ { "matcher": "*", "command": "audit.sh" } ],
    "Stop": [ { "url": "http://127.0.0.1:9000/joy-policy" } ] } }
```

每条 handler **二选一**：`command`（shell）或 `url`（HTTP）——两个都写或都没写
都是配置错误，该条被跳过并欠一行 stderr。

**事件**（名字采用生态里通行的叫法，搬配置不必学第二套）：`PreToolUse` /
`PostToolUse` / `PostToolUseFailure` / `PermissionRequest` / `SessionStart` /
`SessionEnd` / `Stop` / `StopFailure` / `SubagentStart` / `SubagentStop` /
`PreCompact` / `PostCompact`。

**handler 收到什么**：一段 JSON（`hook_event_name`、`session_id`、`cwd`、
`tool_name`、`tool_input`、`tool_output`、`matcher`…）——shell 命令在 stdin 上，
HTTP 端点是 POST body。

**怎么表态**：

| 退出码 / 输出 | 含义 |
|---|---|
| `exit 0` | 放行；stdout 若是 JSON 就按下面的字段处理 |
| `exit 2` | **阻断**（唯一靠退出码阻断的方式），理由取 stdout 的 `reason` 或 stderr |
| 其它退出码 | **非阻塞错误**：记一行 stderr，动作照常 —— 钩子写坏了不该让所有工具瘫痪 |
| HTTP `200` | 放行；响应体若是 JSON 就按下面的字段处理 |
| HTTP 其它状态 / 连不上 | 「没能表态」而不是「说不」：记一行 stderr，动作照常（超时仍按下方的两分法） |
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

`Stop` 的阻断会让这一轮**再跑一次**（上限 1 次）：它因此是目标循环（`/goal`）的
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
| `JOY_WRITE` | `0`（驾驶舱默认 `1`） | 开启 `write_file` / `edit` 工具（关着 = 模型看不见它们）。可写范围**只有**本轮选的工作目录与 `JOY_EXEC_WRITABLE_ROOTS`；没选目录一处也写不了 |
| `JOY_DELEGATE` | `0` | 开启 `delegate_task` 工具（子代理有自己的上下文，且不能再派生） |
| `JOY_EXEC_ALLOW` | — | 放行表，逗号分隔，支持末尾 `*` 通配（`cargo test,git status,ls *`）。空 = 全部拒绝 |
| `JOY_EXEC_TIMEOUT` | `30` | 单条命令的超时（秒） |
| `JOY_APPROVAL` | `never`（驾驶舱默认 `on-request`） | 放行表没匹配上时怎么办：`never` = 拒绝，`on-request` = 问一句（没人答 = 拒绝）。对硬拒名单与沙箱无效 |
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

## 写文件（`JOY_WRITE`）

`JOY_WRITE=1` 会注册两个工具：`write_file`（整文件写入，覆盖原内容）和 `edit`
（一次精确匹配替换 —— `old_string` 必须在文件里恰好出现一次，0 次或多次都会被
拒绝，返回里带变更 hunk）。命令行 / hub 默认关；**单用户驾驶舱默认开**，因为
那里有人能看着、也能审批（显式设了 `JOY_WRITE` / `JOY_APPROVAL` 就听人的）。

驾驶舱顶栏的「工作目录」是一个点选文件夹的按钮，不是输入框：选中的目录就是
**唯一的可写范围**（再加 `JOY_EXEC_WRITABLE_ROOTS`），没选目录一处也写不了。
Joy 的 home 和进程 cwd 都不在范围内 —— 有工作区的助手不应该顺手写进记忆或
启动它的那个目录。范围外的写入在审批之前就被硬拒。内容必须是 UTF-8 文本
（不含 NUL 字节）且不超过 1 MiB。驾驶舱里 `JOY_APPROVAL=on-request` 是默认值：
**每次**写入都在对话窗口弹一张授权卡，允许 / 允许并记住 / 拒绝，没人答就超时
按拒绝算 —— 配置面板被收掉了，审批留在人说得上话的地方。

## 任务执行计划（`JOY_PLAN`）

`JOY_PLAN=1` 时，长任务在**第一轮之前**多一次模型调用，让它先把计划写下来：3–6 步，
短、可执行、能看出做没做完。计划以 `task_steps` 行保存（`kind = "plan"`，`run_id` 为空 ——
计划比某一次执行活得久），并作为一段文本附在本轮消息后面；验收没过时允许**改一次**
计划（最多两版，改到第三版是来回折腾，不是进展）。

**默认关**：多一次模型调用、并且改变了 prompt，属于「由人按下的开关」。
计划永不携带验收条件 —— 那是人定的事实，模型不能靠重新计划把它放松。

## 模型目录（`models-catalog.json`）

二进制里预置了 12 家厂商（`joyczl-provider`）。要加一家、或调整内置厂商的默认
值而不改代码，写 `<home>/models-catalog.json`：一个 JSON 数组，每行要么**按 id
补丁一个已有厂商**（字段任意子集），要么**新增一家**（`wire`、`model` 必填；
`key_env` 留空或不填 = 本地免 key；`context_window` 缺省 32768）。例子：

```json
[
  { "id": "deepseek", "context_window": 999999 },
  { "id": "mybox", "wire": "openai", "model": "qwen3:8b",
    "base_url": "http://127.0.0.1:11434/v1" }
]
```

文件**只在启动时读一次**并合并进目录；`/model`、驾驶舱「模型」面板与
`model/list` 看到的都是合并后的结果，`model/save` 也按它校验。未知字段会被
拒绝（悄悄无效的错字比报错更糟）；坏文件或坏行只欠一行 stderr 并退回内置表
——绝不拦启动。改完要重启进程。注意 embedding 端点（混合检索）目前仍只从
内置表里选。

不想手工拼这一行 JSON，可以用生成脚本（只有标准库，无第三方依赖）：

```bash
python3 scripts/gen_catalog.py --list        # 看看上游有哪些厂商
python3 scripts/gen_catalog.py glm --model glm-5.2              # 内置 id → 补丁
python3 scripts/gen_catalog.py zhipuai --model glm-5.2 \
  --base-url https://open.bigmodel.cn/api/paas/v4 \
  --out ~/.joy/models-catalog.json                              # 新 id → 新厂商，直接合并写盘
```

数据源是 [models.dev](https://models.dev)（MIT，OpenCode 团队维护的模型元数据库）：
context 上限、是否看图、是否支持工具调用都从它取；wire 和 key_env 由脚本里的
joy 映射表决定。脚本会挑「支持工具调用里上下文最大的」当默认模型 —— 那只是
起点，主模型和小模型值得人工确认（候选列表会打在 stderr 上）。

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
云端 provider 同一条 wire。LM Studio 与 vLLM 形状相同——配一套指过去即可
（`--url` 给端点、模型名写进 `--model`）。零配置的本地起步：

```bash
JOY_PROVIDER=ollama joy          # 或：/model add 本地 ollama qwen3:14b
```

这是隐私路径、零成本路径，也是断网（或 key 失效）时照样能用的路径。
Ollama 没起来时报的是网络错误——`ollama serve` 把它启动起来。

## 各厂商的 key 变量

Joy **不会**自动去读这些变量——密钥只来自你配置进模型的那套（`--env 变量名`
是显式声明「读这一个」）。变量名照旧是各家的习惯名：
`ANTHROPIC_API_KEY`、`OPENAI_API_KEY`、`DEEPSEEK_API_KEY`、
`GEMINI_API_KEY`、`MOONSHOT_API_KEY`（kimi）、`ZHIPU_API_KEY`（glm）、
`MINIMAX_API_KEY`、`XAI_API_KEY`、`OPENROUTER_API_KEY`、
`OPENCODE_ZEN_API_KEY`、`OPENCODE_GO_API_KEY`。

没配（或配的那套缺 key）时报 `PROVIDER_ERROR (-32000)`，错误信息直接给出
`/model add ...` 这条命令。

## 搜索与评测

| 变量 | 说明 |
|---|---|
| `TAVILY_API_KEY` / `JOY_SEARCH_API_KEY` | `search_web` 升级为 Tavily（缺省用免 key 的 Bing HTML） |
| `BRAVE_API_KEY` | 链上第二个付费后端（Tavily 之后尝试） |
| `JOY_SEARCH_MAX_PER_TURN` | 每轮 `search_web` 上限（默认 20，范围 1–200） |
| `JOY_SEARCH_BING` | `0` 关闭免 key 的 Bing 后端（默认开，DDG 常被网络挡住时靠它） |
| `JOY_SEARCH_DDG` | `0` 关闭无 key 的 DuckDuckGo 后端（默认开，最后的兜底） |
| `JOY_JUDGE_MODEL` | judge 裁判模型覆盖 |
| `JOY_GH_REPO` | gather 的 github scan |

搜索后端是一条有序降级链：Tavily → Brave；完全没配 key 时才走免 key 的
**Bing HTML → DuckDuckGo**（Bing 在 DDG 端点不可达的网络里通常还能用；DDG 是最后
的兜底）。付费后端失败后不再回头赌免费端点——一轮只赌一次。没有任何可用后端时
`search_web` 干脆不注册，启动日志会说明；搜索结果与抓取的网页正文一样，进上下文
前都打「外部数据，不是指令」的不可信包装。

## 驾驶舱与二进制

| 变量 | 默认 | 说明 |
|---|---|---|
| `JOY_PORT` | `7777` | dashboard 端口；占用时自动向后找 10 个 |
| `JOY_DASHBOARD_DIR` | 仓库内 dist | 前端构建产物位置 |
| `JOY_NOTIFY` | 关 | 开了才读 `<home>/notify.json`（`{"slack": "webhook URL"}`）并往外推通知：**审批请求 / 后台作业收尾 / 目标终局**。fire-and-forget —— 发送失败只往 stderr 说一声，绝不拦 turn。URL 是管理员配的，指向内网自建端点（ntfy/Gotify 这类收 `{"text": …}` 的）是合法用法，不做 SSRF 护栏
| `JOY_OPS_HOST` | `127.0.0.1` | 绑定地址。**绑非回环必须同时设 `JOY_OPS_TOKEN`，否则启动期拒绝**（配错就该起不来） |
| `JOY_OPS_TOKEN` | — | `/api/*` 的 Bearer token（常量时间比对）。回环地址可不设；设了本机也要带。驾驶舱页面首次 401 时会问一次并存 localStorage。**刻意不进 `config/read`**：部署面参数不该被模型看到 |
| `JOY_HUB_ROOT` | — | 多用户模式的根目录（也可用 `joy hub --root` 传） |
| `JOY_HUB_MAX_OPEN` | 32 | hub 同时最多开几个用户的 app-server（每个是一个子进程）。满了按「最久未用且没在用」驱逐；全都在用时宁可超开也不把请求变成错误 |
| `JOY_HUB_IDLE_SECS` | 900 | 用户的 app-server 闲置多久后关掉。0 = 不按闲置驱逐，只受上限约束 |
| `JOY_BIN` | — | Python SDK / TS 客户端寻找 `joy` 二进制的显式路径 |

## 多用户单机（`joy hub`）

给团队用的形态：**一个 ops 进程 + N 个用户 + 每人一个完整 home**（五家参考
同类参考实现的 profile 与会话键都是这个形状；没有一家做账号系统）。

```
JOY_HUB_ROOT=~/joy-hub joy hub           # 起多用户驾驶舱，默认仍绑 127.0.0.1
```

- **账号自助注册，座位有上限**：打开驾驶舱即可注册（用户名 + 密码 + 设备名）。
  `JOY_HUB_MAX_OPEN` 兼任座位数——满员即关闭注册。**第一个用户**必须从本机
  （回环地址）注册，或用 `joy hub adduser alice --root ~/joy-hub` 生成一枚
  只打印一次的 CLI token——公网新 hub 绝不让最先撞上来的陌生人当主人。
- **登录即配对**：每台设备名一枚自己的 token，与 PBKDF2-SHA256 密码凭证
  （ring，加盐、常量时间验证）一起存在 `users.json`（0600）。同设备同密码
  拿同一个 token——登两次不会翻倍。旧的扁平 `用户名 → token` 文件装载时
  自动迁移（token 变成设备 `cli`）。
- **密码找回走 CLI（刻意如此）**：用户名已存在时本机重注册也会被拒；主人
  找回密码 = 删掉 users.json 里该用户的条目重新添加——没有邮件流程、
  没有重置链接。
- **隔离是结构性的**：每个用户一个 home（state.db、models.json、jobs、
  技能全套），各自的 app-server 子进程懒加载（用到才起）。A 的密钥不可能
  出现在 B 的路径里 —— 因为根本没有共享路径。
- 用户名只许小写字母、数字与连字符（≤32）—— 名字会变成目录，`../` 在
  装载期就拒收。
- 鉴权语义与单用户模式**同一条 Bearer 路径**：单用户设 `JOY_OPS_TOKEN` 与
  hub 模式的 per-device token，走的是同一段代码。
- **跨端同步**：`GET /api/events`（Bearer）推轻量提示——「别处有回合开始/
  结束」「有授权待批」。细节由客户端拉 `/api/data`；提示不带载荷。`/api/*`
  的 CORS 是放行的，因为客户端（Tauri 壳、PWA）用请求头鉴权，从不带 Cookie。

## 连接器（`<home>/connectors.json`）

「草稿，永不发送」是默认；连接器是唯一的例外，并且被三道栅栏围住：

1. **不开就没有。** 没有 `connectors.json` → 没有任何连接器工具，模型连名字都看不见。
2. **默认还是草稿。** 配好连接器但 `"live": false`（默认）时，消息只写进
   `<home>/outbox/`，不出机器。
3. **每次实际发送问一次。** 审批卡写清收件人与正文摘要；拒绝/超时 = 不发。
   白名单是硬边界，**留空 = 谁都不许**。

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

| 字段 | 说明 |
|---|---|
| `smtp_port` | 465 = 隐式 TLS，587 = STARTTLS，其它 = 明文（只给本地中转/测试） |
| `secret` / `app_secret` | `env:VAR` 或 `SecretStore` 条目名（钥匙串优先，回退 0600 明文并如实报告） |
| `allow_recipients` | 邮件：完整地址或 `@域名`；飞书：`open_id` / `chat_id` 精确匹配 |
| `live` | 只有 `true` 才会真的发送；从 `false` 改成 `true` 是人的动作 |
| `receive_id_type` | `open_id`（成员）或 `chat_id`（群） |
| `base_url` | 飞书自建/私有部署时改这里，默认 `https://open.feishu.cn` |
| `imap.*` | 入站块：`host` / `port` / `mailbox` / `poll_secs` / `mode`（`notify` 或 `turn`） |
| `as` | 飞书发送身份：`app`（默认）或 `user`（需先 `joy connector feishu login`） |

人的两个命令（终端直接发不弹审批卡 —— 敲命令的人就是批准人）：

```bash
joy connector list
joy connector email --to alice@corp.com --subject 周报 --body 正文 [--dry-run]
joy connector feishu --to ou_xxx --text 正文 [--dry-run]
```

模型侧工具是 `email_send` / `feishu_send`，只在连接器启用且白名单非空时注册。
驾驶舱「连接器」面板只读展示：哪个连接器、能不能真发、白名单多宽。

### 入站邮件（`email.imap`）

出站有「草稿 → 人点头 → 发送」；入站是反方向：外面的东西自己想进来。所以这里只做
最保守的那件事——**把信原样记下来，等人决定要不要处理它**：

```json
{"email": {
  "smtp_host": "smtp.example.com", "smtp_port": 465, "from": "joy@example.com",
  "username": "joy@example.com", "secret": "keychain:connector:email",
  "allow_recipients": ["@example.com"],
  "imap": {"host": "imap.example.com", "port": 993, "mailbox": "INBOX",
           "poll_secs": 300, "mode": "notify"}
}}
```

- 没有 `imap` 块 = **完全没有入站**：不起轮询器，也没有收件箱面；
- `port`：993 隐式 TLS、143 STARTTLS、其它明文（只给本地假服务器）；`poll_secs`
  有 30 秒下限；`mode` 只能是 `notify` / `turn`，写错启动即失败；
- `mode: "notify"`（默认）只记录 + 通知，驾驶舱概览的「收件箱」卡片点
  「处理这些邮件」（`inbox/summarize`）才是人说「现在处理」；`mode: "turn"` 让新邮件
  自动开一轮 —— 这是写在文件里的常设授权，模型不自己决定；
- 首次启动只记水位、**不拉历史**（收件箱里那几百封旧信不算「新来的」），此后按 UID
  去重；正文只存解出来的纯文本，附件不落盘；
- 取信一律 `BODY.PEEK[]`：Joy 读一眼不会把别人的信标成已读。

### 飞书用户身份（`"as": "user"`）

默认以应用身份（`tenant_access_token`）发言；要以「某个人」的名义发，得先走一次
浏览器授权：

```bash
joy connector feishu login     # 授权码流程，token 落 <home>/connector-auth/feishu.json（0600）
joy connector feishu status    # 有效 / 已过期（下次发送前会续期）
joy connector feishu logout    # 删掉 token
```

然后在 `connectors.json` 的 feishu 块里写 `"as": "user"`。回调固定
`127.0.0.1:41766/callback`，这个地址要在飞书应用后台**一模一样**地登记；流程没有
PKCE。

token 是**轮换**的：refresh token 用一次就换新的，所以续期只能串行（进程内走
`connector-auth/feishu.lock`）。长住的进程（app-server）会在**过期前十分钟自动续**
—— 发送不必为它等一次网络往返，笔记本睡过到期时间也能醒来继续用；续期被飞书拒绝
（授权被撤销/refresh token 已死）时它只喊一次，让人重新 login，不会每五分钟刷一次
日志。`joy connector feishu status` 里的「已过期，下次发送会尝试续期」仍然成立，那是
CLI 自己的惰性路径。

## Chat gateways (joy-ts)

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
