# ⑩ 命令行：`joyczl-cli`

一个二进制，八种用法。这一层不藏逻辑——它把 ④⑤⑥⑦⑧⑨ 各层接起来，并决定
「同一个引擎的哪一面朝外」。

目录：`joy-rs/joyczl-cli/src/`（`main.rs`、`repl.rs`、`gather.rs`、`schedule.rs`、
`memory_cmd.rs`、`skill_cmd.rs`、`mcp_cmd.rs`）。

## 10.1 分发与 `home()`

```rust
main() → match args().nth(1) {
    None                     → REPL（进程内 Server）
    Some("app-server")       → run_stdio（协议服务端）
    Some("dashboard")        → joyczl_ops::serve（HTTP + SSE）
    Some("gather")           → 晨报
    Some("schedule")         → 常驻调度
    Some("memory") / ("skill") / ("mcp") → 各自的子命令
    Some("eval") / ("judge") → 评测（exit code 即结论）
    Some("--version"|"-V")   → "joy {CARGO_PKG_VERSION}"
    Some(other)              → bail（并打印 USAGE）
}
```

`home()` 的优先级：`JOY_HOME` 环境变量 → 否则 `"./.joy"`（**当前工作目录**，不是
`~/.joy`）。这让「在项目目录里跑 Joy」自带一个项目级状态目录，也让测试能用
`JOY_HOME=/tmp/x` 完全隔离。

`app-server` 分支的注释值得一读：`settings.home = home()` 是**显式传参赢过环境
变量**，于是 `JOY_HOME=/tmp/x joy app-server` 与脚本里的行为完全可预期。

## 10.2 `repl.rs`：终端对话

`joy` 裸跑的入口。结构很简单，但有两个细节值得学：

**并发结构**：`run_turn` 跑在 `tokio::spawn` 里，REPL 同时消费 `Frame` 通道。

```rust
let (sink, mut rx) = EventSink::channel();
tokio::spawn(run_turn(&server, params, RequestId::Number(0), &sink));
while let Some(frame) = rx.recv().await { … }
```

于是「流式渲染」不需要任何回调：**帧到了就画**。

**每种帧怎么渲染**（这就是终端体验的全部）：

| 帧 | 渲染 |
|---|---|
| `TextDelta` | `print!("{delta}")` + flush（字打出来就有，不等整段） |
| `GateDecided` | 先断开流式行，再打小字「…记忆门：检索/跳过（reason）」 |
| `ToolStarted` | 「…正在调用 {tool}…」 |
| `ToolCompleted` | 「…{tool} 完成（ok/出错，{ms}ms）」 |
| `ConsolidationCompleted` | 「…提炼出 {n} 条新事实」 |
| `TurnCompleted` | 断开流式行；若一段流式文本都没有（快答路径）就整段打 `reply`；再打「— {model} · {iterations} 轮 · {latency}ms · token {i}+{o}」；被打断时补一句 |
| 图事件、`Error` | 忽略（图事件在 meta 里，错误由 task 的 Err 分支报） |

`end_streamed_line()` 是个小助手：只有在**真的流过文本**时才换行并把计数清零——
否则小字会黏在回复尾巴上。

**斜杠命令**：`/help`（打印 HELP）、`/new`（换一个新 session id）、`/sessions`
（列会话）、`/memory [词]`（无词列最近 10 条事实 + 5 条情景，有词就检索）、
`/quit` `/exit` `/q`。不认识的 `/x` 会明确说「不认识的命令」。

`chat_turn` 结束后 `task.await` 的 `Err` 会打出 `error.message`——覆盖「turn 自身
失败」与「没配 key」两种情况（后者的消息来自 ④ 的 `no_key_message`）。

### 批准问答（`ask_approval`）

终端是唯一「当场就能回答」的界面，所以它直接在通知渲染循环里读 stdin：

```text
…需要批准：echo approved-ok
  为什么问：没有匹配的放行规则。当前规则：ls *。…
  批准执行？（y = 允许 / a = 允许并记住 / 其他 = 拒绝）:
```

三个决定：

* **单独开一个 stdin 句柄**，而不是把外层的 `BufReader` 传进来：主循环此刻正
  等着这一轮跑完，没人在读，两个句柄不会打架（为这点事做异步 stdin 竞速不值）。
* **含混的回答一律算拒绝**（空行、乱打、EOF）—— 与整个机制同一条规矩：放行只有
  一种来源，就是一个明确的「可以」。
* **回答太晚要说出来**（`accepted: false` → 「这个问题已经过期了」），否则用户
  会以为自己批准成功了，而那一轮早就不等了。

## 10.3 `gather.rs`：把四路真实数据接进 gather 图

这个是「注入闭包」的教科书用法——图（⑧）不认识任何具体实现，六个闭包在这里
接上真实世界：

| 闭包 | 真实实现 | 失败时 |
|---|---|---|
| github | 读 `JOY_GH_REPO` → `gh -R {repo} pr list --state open --limit 20` 与 issues 同理；数非空行 | 没配 → `Err("JOY_GH_REPO 没设…")`；`gh` 非零退出 → `Err("gh 失败：{stderr}")` |
| web | **独立**开一个 state，`joyczl_tools::web::search_web()` 当**库**调（不走模型的工具开关） | `map_err` 成字符串 |
| calendar | 复用 `triage::todays_events(&home)` | 空/`(nothing today)` → 计数 0 |
| memory | `server.facts().search("project repo contributors release", 8)`，**不过检索门**（晨报早知道要查） | 空 → `"(nothing relevant)"` |
| synth | `server.resolved().client.create(synth_request(model, state))` | 没配 key → `Err` |
| draft | 写 `<home>/outbox/gather-{日期}.md` | IO 错 → `Err` |

两个设计点：

- **github 诚实报缺配**：没设 `JOY_GH_REPO` 时不是「没有 open PR」，而是
  `unavailable (JOY_GH_REPO 没设…)`——人在简报里看得出是「没配」而不是「没事」。
- **web 那一路绕开工具开关**：`JOY_*` 工具开关管的是「模型能不能搜」；这里是
  Joy 自己的代码在问，所以直接调工具函数。

跑完打印 digest、草稿路径与 `report.errors` 里每条 `{node}: {error}`。

## 10.4 `schedule.rs`：声明式定时任务

### 两条声明来源（`load_jobs`）

1. **技能**：`loaded_skills(home)` 里带 `schedule:` 的，变成一条 job，
   prompt 是「按技能 '{name}' 的做法执行一次：{description}」。
2. **`<home>/schedules.json`**：`{"jobs":[{"name","cron","prompt"}]}`；
   缺字段的条目打一行 stderr 跳过，JSON 坏了也跳过——**一条写坏的声明不该让
   整个调度器起不来**。

于是「定时任务」不需要另立一套格式：技能本来就是「怎么做事」，加一行时间就成了
「每周一早上八点这么做」。

### cron：手写五字段匹配

`cron_matches(cron, at)`：必须恰好 5 段（分 时 日 月 周），否则 false；
周用 `num_days_from_sunday()`（0..6）。

`field_matches(field, value, min, max)` 支持的语法：

| 写法 | 含义 |
|---|---|
| `*` | 任意 |
| `1,8,20` | 列表（任一段命中即可） |
| `*/15` | 步长：`value.is_multiple_of(15)`（step 必须 >0） |
| `9-17` | 闭区间 |
| `7`（周字段） | 归一成 0（周日两种写法都认） |

不支持的写法（四字段、`1-`、香蕉）一律返回 false——**不猜**。为这一个字段引
一个 cron crate 不值得。

### 去重与循环

```rust
pub fn due_now(jobs, at, fired) -> Vec<&Job> {
    let minute_key = at.format("%Y-%m-%dT%H:%M");
    jobs.filter(|j| cron_matches(&j.cron, at) && fired.get(&j.name) != Some(&minute_key))
}
```

`fired: HashMap<任务名, 上次触发的分钟>`，于是**同一任务同一分钟只触发一次**
（即使 tick 是 20 秒一次、进程卡了一下）。

主循环：每 20 秒一次 tick，**每次重新 `load_jobs`**（等于热加载——技能改了不用
重启）→ 对 `due_now` 的结果**串行** `fire(...).await`。串行是刻意的：同一分钟的
多条任务依次跑，定时任务打爆模型不是「快」，是没人读得过来。

### `fire`：跑一轮，结果进 outbox

```rust
EventSink::channel() → TurnStartParams{ session_id: "schedule:{job.name}", message: prompt, stream: true }
tokio::spawn(run_turn(...)) → 收帧直到 TurnCompleted 取 reply
→ 写 home/outbox/schedule-{name}-{%Y%m%d-%H%M}.md：
   # {name} / _由 joy schedule 在 {时刻} 按 cron '{cron}' 触发_ / ## 请求 / ## 回答
```

会话按任务名分开（`schedule:tick`），于是**每次执行带着同一个任务的历史与滚动
摘要**，任务之间互不串味；产出是可以慢慢读的文件，不往聊天里塞。

`docs/limitations.md` 里记着两条：按分钟粒度、漏掉的那一分钟不补跑。

## 10.5 `memory_cmd.rs`：`joy memory reindex`

```
Embedder::from_settings(settings)?        ← 先建 embedder（配错立刻报错，别等到改库一半）
state::open(home/state.db)
if missing_embedding(1) 为空 → 「所有事实都已有向量，不需要补。」
否则 retrieval::reindex(&facts, &embedder) → 「补了 {n} 条向量。」
```

顺序是刻意的：**先校验配置再开库改数据**。

## 10.6 `skill_cmd.rs`：list / export / install / update

| 子命令 | 行为 |
|---|---|
| `list`（默认） | 一行一个：`- {name}{ v{version}}  {description}` |
| `export` | `--to claude,codex`（默认 claude）→ `~/.claude` / `~/.codex`（`--project` 时用当前目录）下的 `skills/<名字>/`；`--force` 才覆盖对方改过的副本；`--names a,b` 过滤输出 |
| `install <url\|路径>` | `raw_url` 转换后抓取（15 秒超时）或读本地文件 → `skills::install_from_text`（**从不覆盖**） |
| `update [索引]` | 索引默认 `<home>/skills/index.json`，也可以是路径或 http(s)；`parse_index` → `update_all` → 逐条打印结论，有失败则 bail |

`raw_url` 解决的是「人复制来的多半是浏览器地址栏里那个」：

```
github.com/…/blob/…  → raw.githubusercontent.com/…/…（去掉 blob）
gist.github.com/…    → 追加 /raw
其它                 → 原样
```

`install` 与 `update` 的分工是有意的：**install 从不覆盖**（技能是指令，装之前先
读一遍），**update 按版本替换**（先备份进 `.backup/`，先落 `.staging/` 再原子换）。
两条路径共用 `is_slug` 与 `parse_skill_text`（⑦ 里说的那次分叉就是它们）。

## 10.7 `mcp_cmd.rs`：list / login / serve

| 子命令 | 行为 |
|---|---|
| `list`（默认） | 读 `mcp.json`，逐条打 `name`、`url`/`command`、鉴权方式（oauth / auth_env / 无） |
| `login <名>` | 找 spec → 必须有 `url` → 构造 `open_browser`（macOS `open` / Windows `cmd /c start` / 其它 `xdg-open`，`spawn` 不等待、失败不报错）→ `oauth::sign_in` → 打印 token 路径与前 8 位 |
| `serve` | 开 state.db → `MemoryServer::new(Facts, Episodes).run_stdio()` |

**登录是唯一会开浏览器的地方**：app-server 启动时缺 token 只警告并跳过该服务器，
绝不擅自弹浏览器——「一轮对话绝不擅自执行」的同一条规矩。

## 10.8 测试

CLI 的 7 条测试都在 `schedule_tests.rs`：cron 语义（含 `*/15`、`9-17`、`1-5`、
周日的 `7` 与 `0`）、坏 cron 永不触发、同一分钟只触发一次、两条来源都能装载任务、
以及一条**端到端**：装一个 scripted provider 的 Server，调 `fire()`，断言回复落进
`outbox/schedule-{name}-{时刻}.md` 且会话是 `schedule:tick`。

（其它子命令是薄胶水层，靠 ⑫ 的冒烟脚本覆盖。）
