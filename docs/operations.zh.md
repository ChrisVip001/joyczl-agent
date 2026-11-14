# 运行手册

[English](operations.md) | 简体中文

日常运行的每一种形态、推荐的环境变量与常见故障。

## 支持的平台

| 平台 | 状态 | 原因 |
|---|---|---|
| macOS（Apple Silicon / Intel） | ✅ 一等公民 | seatbelt 沙箱、钥匙串、`sh` hooks |
| Linux | ✅ 一等公民 | bubblewrap 沙箱、secret-service、`sh` hooks |
| Windows（原生） | ❌ 拒绝启动（exit 2） | 没有沙箱后端——按「无沙箱不执行」铁律 `run_command` 永远跑不了；hooks 要 `sh`；凭据库读不回密钥，只能落明文 `secrets.json` |
| Windows + WSL2 | ✅ 用 Linux 版即可 | 上面的一切都是真的 Linux 用户态 |

拒绝发生在 `joy` 二进制启动时（`cfg!(windows)`），原因直接打印——
「看起来健康但处处受限的半成品」不如一声清楚的不。CI 保留 Windows job
但只做 build + 单测（沙箱与 hooks 测试照旧跳过）；库在哪儿都能编译，
二进制只是不启动。

## 终端对话

```bash
cd joy-rs && cargo build
JOY_PROVIDER=deepseek DEEPSEEK_API_KEY=… ./target/debug/joy
```

流式输出；`/memory [词]` 查记忆、`/sessions` 列会话、`/new` 新会话、
`/quit` 退出。中断：`Ctrl-C` 在终端里是退出进程——网页端才用
`turn/interrupt` 打断。

## 网页驾驶舱

```bash
cd joy-ts && npm run build --workspace @joy/dashboard   # 前端只需构建一次
JOY_HOME=$HOME/.joy ./target/debug/joy dashboard        # → http://localhost:7777
```

首屏展示配置 / 会话 / 记忆；聊天区实时呈现检索门判定、工具调用与流式
回复。端口被占自动 +1（最多 10 个）。

- **页面记得你在哪。** 当前打开的是哪个会话存在 `localStorage`（`joy.session`，和工作目录
  放在一起），刷新回到的还是它，而不是最老的那一个；如果记住的会话已经不存在，就退到最新的
  一条 —— 不会给你一个列表里都没有的空壳。
- **切回去还在你上次读到的地方。** 每个会话各自记着「离底部多远」（在内存里，按页面加载
  计）：读着长对话切走、再切回来，还是那一行，不用再拖。这次加载里**第一次**打开的会话
  落在最新一条（刷新也一样）；位置是瞬时放好的，不会从第一句滑下来。
- 跨设备心跳（另一台设备跑完了一轮）只重读历史、**不动滚动位置**：你正翻着上面的内容，
  就停在那里。
- **助手自己的话是英文。** `SOUL.md`、提示词各段、工具结果文案、计划器提示词都是英文 ——
  不管你说什么语言，它用你的语言回答。`<home>/SOUL.md` 归你：原样使用，只有与内置种子**逐字
  相同**时才会被替换。（从旧版本带过来的灵魂会被当成「你写的」，所以要换提示词语言得自己
  改一次那个文件。）
- **中英双语。** 侧栏右上角的按钮切换中文 / English，并记住选择；全新的浏览器跟随
  `navigator.language`（`zh-*` 取中文，其余取英文）。切换会刷新页面 —— 文案是在绘制时写进
  DOM 的。模型输出、会话标题、任务正文保持原样：那些是数据，不是标签。

### 放到一台服务器上（自托管）

```bash
export JOY_OPS_HOST=0.0.0.0            # 非回环地址必须配 token，否则拒绝启动
export JOY_OPS_TOKEN=$(openssl rand -hex 16)
export JOY_HOME=/srv/joy/.joy
joy dashboard                          # → http://<服务器>:7777（/api/* 需要 Bearer token）
```

- 页面首次 401 时会问一次 token，之后存在浏览器 localStorage；静态页本身
  不需要 token（先能加载，才能问）。
- **key 不用拷文件**：`models.json` 里写 `"key_source": "env:DEEPSEEK_API_KEY"`，
  在服务器的环境里 export 这个变量即可。key 只在跑那一轮的进程内存里被读到，
  永不落盘，也永远不会出现在 `/api/*` 的应答里。
- TLS 交给反向代理（Caddy / nginx 终结 HTTPS），Joy 自己只说 HTTP。
- 产品边界：服务器跑的是**编排**；连接器与 MCP 服务器留在用户自己的设备上
  （本地优先 / 无沙箱不执行），不要把用户自配的外挂放上共享服务器。

## MCP 外挂

写 `<home>/mcp.json`（stdio 或 HTTP 服务器），工具自动并入工具表。
远程服务器用 `"oauth": true` 时先 `joy mcp login <名>`；token 过期自动
刷新，没有 refresh_token 时重跑 login。

驾驶舱的「能力中心」是同一个文件的图形门：加 / 替换 / 删一个连接器，
传输与鉴权规则和服务端同一套（`url` 与 `command` 二选一；`auth_env` 是
变量名，不是密钥）。改动重启 Joy 后生效。要发整机共用的服务器，设
`JOY_MCP_CONFIG`——那些行在能力中心里带「内置」标签置灰，所有写操作
一律拒绝。

运维侧的已知边界（没有日志轮转、不做 daemon 化、漏掉的定时执行不补跑等）见
[limitations.zh.md](limitations.zh.md)。

### 把 Joy 的记忆给别的 agent 用

反方向也有：`joy mcp serve` 让 Joy 自己成为一台 stdio 的 MCP 服务器，暴露
五个记忆工具（`memory_search`、`memory_remember`、`memory_forget`、
`memory_list`、`memory_episodes`），这台机器上的其他 agent 因此读写同一份
事实。**只暴露记忆**——不暴露任何「替别人动手」的能力。

```json
{"mcpServers": {"joy-memory": {
  "command": "joy",
  "args": ["mcp", "serve"],
  "env": {"JOY_HOME": "/Users/you/.joy"}
}}}
```

任何 MCP 客户端（Claude Code、codex……）配上这一段就与 Joy 共享长期记忆。
服务器用同一个 `open()` 开同一个 `state.db`（WAL + busy_timeout，多进程本来
就能共存）；别的 agent 写进来的事实标着 `source: mcp`，来源始终可查。

## 晨报（cron 友好）

```bash
JOY_GH_REPO=owner/repo joy gather
```

四路并行扫描 → 一次综合 → 摘要打印、草稿写进 `outbox/gather-<日期>.md`。
单个数据源失败变成报告里的 "unavailable"，不影响整体。

## Python SDK 嵌入

```python
import os
from joyczl_agent import JoyClient

async with await JoyClient.connect() as client:   # 拉起 joy app-server 子进程
    client.on_notification(lambda n: print(n.type))
    await client.request("turn/start", {"message": "你好", "stream": True})
```

环境变量由嵌入方进程决定（子进程继承）；`JOY_BIN` 可指定二进制。
打包：`just build-python-bin` 产出分平台 wheel。

## 打包

```bash
docker build -t joy .
docker run --rm -p 7777:7777 -v joy-state:/home/joy/.joy \
  -e ANTHROPIC_API_KEY=… joy                 # 驾驶舱
docker run --rm -it -v joy-state:/home/joy/.joy \
  -e ANTHROPIC_API_KEY=… joy app-server      # stdio 上的协议
```

全部状态都在 `JOY_HOME` 下（镜像里是 `/home/joy/.joy`），一个 volume 就能带着走。
镜像里装了 `bubblewrap`——Linux 沙箱后端要的就是它，没有它 `run_command`
会拒绝执行任何命令（这是设计，不是故障）。Homebrew formula 模板在
`packaging/homebrew/joy.rb`（用 `cargo install` 从源码构建；发布 tarball 的
sha256 自己填）。

## 定时任务

`joy schedule` 是常驻进程，按五字段 cron（`*`、`*/步长`、`a-b`、`a,b`）触发
声明式任务。两种声明方式：

* 技能的 frontmatter 里带一行 schedule：

  ```markdown
  ---
  name: weekly-review
  description: 汇总这一周并起草周一简报
  schedule: 0 8 * * 1
  ---
  ```

* `<home>/schedules.json` 里的一条：

  ```json
  {"jobs": [{"name": "standup", "cron": "0 9 * * 1-5", "prompt": "今天有什么安排？"}]}
  ```

每次触发都在自己的会话里跑完整一轮（`schedule:<任务名>`），所以每条任务带着
自己的历史与滚动摘要；结果写进 `<home>/outbox/schedule-<名>-<时刻>.md`，
而不是往聊天里塞。同一分钟最多触发一次，任务串行执行，技能改动不用重启就
生效。交给 launchd/systemd 常驻 —— 或者干脆不用它，用系统 cron 直接调
`joy gather`。

两种声明都会出现在驾驶舱的能力中心（**抽屉 → 定时**），和技能、MCP 连接器并排：一行一个
任务，带 cron、提示词、下次触发、上次触发和投递方式。来自技能的那些行是灰的、只读 ——
那条任务**就是**那个技能，要删就去改 `SKILL.md` 或删技能。新增/删除文件任务只写
`schedules.json`：

```
name      morning-news        小写字母、数字、连字符（它同时是日程的键）
cron      0 8 * * 1-5         五段：分 时 日 月 周
prompt    给我一份今天的科技简报   一句话 —— 它会变成一轮对话
delivery  outbox | notify | quiet
catch_up  错过了补跑一次（默认），还是放弃
```

这块面板是**文件的编辑器**，不是调度器。`joy schedule` 仍然要在跑（或者有别的东西去调
`joy gather`），面板底部就写着这句话 —— 一个悄悄依赖常驻进程的表单，比没有表单更糟。

面板不是唯一的门：直接在对话里说「每天八点给我一份科技简报」也行 —— 模型会调
`schedule_job`，走同一个校验写同一个文件，回答里带上「下次触发」的时间，并提醒
`joy schedule` 得在跑。删是 `cancel_schedule`，列是 `list_schedules`。

为什么要做成工具、而不是「让它自己去写文件」：`<home>/schedules.json` 在每个工作区之外，
`write_file` 拒绝得对，于是界面只好问你选哪个目录 —— 而这个文件和你项目毫无关系。0.17
起它就是 Joy 自己的状态，由管它的工具来写，不再有人问你要目录。

## 数据与备份

`<home>/` 下值得备份的只有几样：`state.db`（记忆与对话，事实来源）、
`SOUL.md`（人格）、`skills/`（过程记忆）、`mcp-auth/`（OAuth token，
0600）。`traces/`、`usage.jsonl`、`MEMORY.md`、`outbox/` 是可再生成的
派生物。

## 故障排查

| 症状 | 处置 |
|---|---|
| `PROVIDER_ERROR (-32000)` 缺 key | 按 .env→环境变量链路补 key 后重启进程 |
| 401 / 403 | key 失效或地区不可达；`/model add <名字> <厂商> --key 新的` 换掉它 |
| MCP 工具不见了 | 看 stderr 警告：连不上 / 未登录（`joy mcp login`）/ mcp.json 坏 |
| 回答里 `[tools used: …]` 重复 | 属预期折叠标记，说明工具活动已入历史 |
| 想知道"它当时为什么这么答" | 打开 `traces/<日期>.jsonl` 对应 turnId 那一行 |
| 端口 7777 被占 | dashboard 自动 +1，启动日志里有实际端口 |

## 压缩与看记忆

* REPL 里的 `/compact [重点]` —— 或驾驶舱「记忆」页的**现在压缩** —— 现在就把较早的轮次
  折进滚动摘要，最近两轮留在上下文里。重点是给摘要材料加一句话（「盯着迁移方案」），
  不是换一套提示：摘要必须逐轮可比。
* 记忆中心（抽屉 → 记忆）三个只读视图。**账本**：一轮一行，点开看这一轮的条目流；
  `折叠` 标的是回填来的轮次，那几轮的工具输出只以摘要形式存在。**记忆库**：上面目录、
  下面正文，`ledger:…` 的 chip 能跳回这页是从哪几轮提炼出来的。**上下文**：六段分层条
  （灵魂 / 记忆 / 技能 / 工具 / 历史 / 摘要），下面是已用、预算，以及到
  `JOY_CONTEXT_WARN_RATIO` 时那句提示。
* 那条带子量的是**会随会话增长的部分**。有四样东西每轮都在提示里、但故意不计入：时钟、
  待办板、能力缺口说明（`JOY_EXEC` / `JOY_WRITE` 没开时那两三行），以及从工作目录到 `.git`
  之间读到的**项目约定**（`AGENTS.md` / `AGENTS.override.md` / `CLAUDE.md`）。前三样各几十个
  token；约定封顶 32 KB —— 所以仓库里有一份很厚的 `AGENTS.md` 时，带子会**低估**真实占用一个
  文件。这是取舍：带子回答的是「一个会话里什么在长」，而按目录恒定的文件不长。
* 一轮最多花 `JOY_MAX_ITERATIONS` 次模型调用（默认 32）。**最后一次是收尾轮**：工具被收起，
  模型只能用三句话说「完成 / 卡住 / 下一步需要什么」。所以长回合的结尾是一句你能接着用的
  话，而不是一句道歉。过长的工具结果在每次请求前从最旧的开始换桩
  （`JOY_TOOL_RESULT_TOTAL_CHARS`），一条话多的命令撑不满整个窗口。
* `joy memory rebuild --page <slug>` 从来源轮次整页重生成。它故意是一条 CLI 命令：
  整页重生成是一次模型调用，模型调用应该是你要求的，而不是按钮偷偷花掉的。

## 崩溃恢复有年龄上限

checkpoint 停在 `open` 意味着上一个进程死在回合中途。启动时 Joy 会接着跑 —— 那份活是人
交代的，续上它不算「模型给自己派活」。0.17 起这条规则有了窗口：`JOY_RESUME_MAX_AGE`
（分钟，默认 `60`）限定这一行最多能有多旧；设成 `0` 就关掉上限、回到「一律续上」。

为什么是一小时而不是「永远」：checkpoint 是崩溃恢复，真实跨度是秒到分钟。续跑一个两天前
的回合已经不是「同一个回合接着跑」—— 两天前问「今天有什么科技新闻」，两天后给出的答案
回答的是一个已经没人在问的问题，而且为此真的花掉一次模型调用和一次抓取。超龄的行会标成
`abandoned`，启动日志里说清楚：

```
(joy) 对账：回合 t… 停在 2 天前（上限 1 小时），没有自动续跑 —— 要接着做，把那句话再说一遍
```

账本里那行照样留着：`abandoned` 是一个状态，不是删除。

## 定时任务恢复与投递

`catch_up` 只补未领取的漏触发。已领取但未结算的触发可能已经产生外部效果，重启不重跑，并保留领取以阻止后续分钟覆盖。先停止常驻调度进程，核对实际效果，再执行：

```sh
joy schedule acknowledge morning-news
```

这只释放已核对的旧领取，不重跑它；未来合法触发可继续。未核对的回合不要通过删库、改时间或重复开始绕过。领取之前落盘失败时不会调用模型。

结果、结算与投递意图保存在 SQLite。outbox 文件未写成功时，用已存内容重试投递；不同的已有文件保留待核对，不覆盖人工修改。启用通知时，文件落地后才向原配置端点入队；失败重启重试，不静默换端点。`produced` 表示已产出，不代表领域验收或用户已读。

## 等待任务输入

在**开始任务**前，选择**等待输入**并说明所需数据，可选设置最晚到达时间。**提供输入**会保存数据；收到数据本身不满足验收条件。任务保留草稿等待，输入到达后恢复，原会话仍可继续聊天。**暂停**阻止自动唤醒，数据仍会保存，只有**继续**才明确授权恢复。继续请求最多等待旧执行收尾十秒；被拒绝时保留暂停状态。

SDK 宿主使用鉴权的 `task/wait` 和 `task/signal` 方法，详见[协议](protocol.zh.md#持久化外部输入)。重试提交时保持事件 ID 和数据不变。输入期限仅限制到达时间；按时收到的数据可以在期限后使用。正常重启运行时即可恢复已提交的事件和草稿；未知工具副作用仍须先核对。

## 核对未知操作

对于已暂停或需要确认的任务，选择**核对操作**。检查真实外部服务或文件，选择已观察到的结果，填写支持证据或外部回执。传输错误本身不能证明没有执行。服务端拒绝保存时表单保留证据。记录结果后任务仍停止；全部未决操作结算后，另行选择**继续**。

SDK 宿主通过[操作协议](protocol.zh.md#操作日志与人工核对)检查和结算。每结算一个操作后重新读取检查点摘要。核对不可伪造 `toolsOk` 成功测量，也不构成服务端幂等保证。
