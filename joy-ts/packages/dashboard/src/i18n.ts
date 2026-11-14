// Two languages, one source: the Chinese string is the key.
//
// The cockpit was written in Chinese and the strings sit where they are used — inside
// `render.ts`'s templates and `main.ts`'s status lines. Pulling them into symbolic keys
// (`sessions.empty.title`) would mean inventing ~220 names and then hunting a typo through
// a table; wrapping the text itself (`t("还没有会话")`) keeps the call site readable and
// makes an untranslated string *visible in the code*, not just at runtime.
//
// `en` is the complete dictionary. A key missing from it falls back to Chinese and logs
// once — a half-translated screen that still works, never a blank button.

export type Lang = "zh" | "en";

const STORAGE_KEY = "joy.lang";

/** DICT[中文] = English. Every user-visible string in render.ts / main.ts / index.html. */
const DICT: Record<string, string> = {
  "核验时的文件版本：{hash}": "File version measured at verification: {hash}",
  "由旧检查点重建；无法确认是否已发送。": "Recovered from an earlier checkpoint; transmission is unknown.",
  "执行前已保存意图；这不证明外部操作已完成。": "Intent was saved before execution; this does not prove the external effect completed.",
  "重新读取操作": "Reload operations",
  "关闭": "Close",
  "核对操作": "Review operations",
  "核对聊天操作": "Review chat operations",
  "当前会话的操作记录": "Operations in this conversation",
  "选择核对结果": "Choose the observed outcome",
  "已确认外部效果成功": "External effect confirmed",
  "已确认外部操作失败": "External operation failed",
  "已确认未执行": "Confirmed not executed",
  "正在加载操作…": "Loading operations…",
  "记录核对结果": "Record reconciliation",
  "待核对操作": "Operation to review",
  "核对结果": "Observed outcome",
  "核对证据或外部回执": "Evidence or external receipt",
  "先检查真实外部状态。记录结果不会执行或继续任务，也不代替原验收。": "Check the actual external state first. Recording evidence does not execute or resume the task, or replace its acceptance checks.",
  "尚有 {count} 个操作需要核对": "{count} operations still need review",
  "没有待核对操作；继续仍需明确授权。": "No operations need review. Continuing still requires explicit authorization.",

  "等待资料": "Waiting for input",
  "等待后台作业": "Waiting for background jobs",
  "提供资料": "Provide input",
  "需要哪些资料？": "What input is needed?",
  "资料内容": "Input",
  "先保存等待条件，再开始任务。等待期间可以继续聊天。": "Save this condition before starting the task. You can keep chatting while it waits.",
  "资料到达后会继续检查任务；收到资料不代表验收通过。": "Once input arrives, Joy continues and checks the task. Receipt alone does not verify completion.",
  "资料最晚到达时间（可选）": "Latest input arrival (optional)",
  "保存等待条件": "Save input condition",
  "提交资料": "Submit input",
  "正在保存…": "Saving\u2026",
  "资料已收到": "Input received",
  "资料尚未到达": "Input has not arrived",
  "由你提供": "From you",
  "来源：{source}": "Source: {source}",
  "最晚到达：{time}": "Latest arrival: {time}",
  "查看资料": "View input",
  "开始任务": "Start task",
  "排队中": "Queued",
  "已产出，未核验": "Produced, not verified",
  "登录 Joy": "Sign in to Joy",
  "服务器地址（默认本机）": "Server address (this machine by default)",
  "用户名（小写字母数字连字符）": "Username (lowercase letters, digits, hyphen)",
  "密码（至少 8 个字符）": "Password (at least 8 characters)",
  "设备名": "Device name",
  "连接你的 Joy 服务器。首次使用会注册一个新账号。": "Connect to your Joy server. First use registers a new account.",
  "登录": "Sign in",
  "注册新账号": "Register a new account",
  "这台 Joy 的 API 需要 token（服务端的 JOY_OPS_TOKEN）": "This Joy's API needs a token (the server's JOY_OPS_TOKEN)",
  "服务端什么都没回（HTTP {status}）—— 多半是页面比二进制新：重新 `cargo build -p joyczl-cli` 再启动一次驾驶舱。": "The server returned nothing (HTTP {status}) — most likely the page is newer than the binary: `cargo build -p joyczl-cli` and start the cockpit again.",
  "服务端的回答不是 JSON（HTTP {status}）：{body}": "The server's answer is not JSON (HTTP {status}): {body}",
  "{steps} 步活动 · {latest}": "{steps} steps of activity · {latest}",
  "会话": "Session",
  "事实": "Facts",
  "情景": "Episodes",
  "怎么想的": "How it decided",
  "小模型": "Small model",
  "工作记忆": "Working memory",
  "最近 {historyTurns} 轮": "Last {historyTurns} turns",
  "提炼频率": "Distillation cadence",
  "每 {consolidateEvery} 轮": "Every {consolidateEvery} turns",
  "检索条数": "Hits per search",
  "最多迭代": "Step budget",
  "最近会话（当前：{session}）": "Recent sessions (current: {session})",
  "还没有会话。说第一句话就有了。": "No sessions yet. Say something and one appears.",
  "收件箱 · {unseen} 封等你处理": "Inbox · {unseen} waiting",
  "收件箱": "Inbox",
  "处理这些邮件": "Handle these emails",
  "只看不标已读；处理是把它们交给一个回合，不会替你回信。": "Read-only, nothing is marked as read; handling hands them to a turn — it will not reply for you.",
  "邮件": "Email",
  "飞书": "Feishu",
  "连接器": "Connectors",
  "可发送": "Can send",
  "草稿": "Drafts only",
  "白名单空 —— 谁都不能收": "Allowlist empty — nobody can receive",
  "白名单 {allowRecipients} 个": "{allowRecipients} on the allowlist",
  "开关在 connectors.json 里 —— 只有人能改；实际发送每次都会问。": "The switch lives in connectors.json — only a person can flip it, and every send asks again.",
  "等待开始": "Waiting to start",
  "进行中": "Running",
  "已暂停": "Paused",
  "需要你确认": "Needs your call",
  "已完成": "Done",
  "未达成": "Not reached",
  "已取消": "Cancelled",
  "任务": "Task",
  "暂停": "Pause",
  "继续": "Resume",
  "取消": "Cancel",
  "待办": "To-do",
  "尚未检查": "Not checked yet",
  "验收": "Acceptance",
  "还有 {rest} 条": "{rest} more",
  "交付物": "Deliverables",
  "还有 {rest} 个": "{rest} more",
  "时长": "Elapsed",
  "工具调用": "Tool calls",
  "预算": "Budget",
  "执行 {runCount} 次": "{runCount} runs",
  "还没执行过": "No runs yet",
  "上次停在「{lastStopReason}」": "Last stopped at “{lastStopReason}”",
  "未设预算": "No budget set",
  "新会话": "New session",
  "还没有会话": "No sessions yet",
  "出错了": "Something went wrong",
  "{iterations} 轮": "{iterations} steps",
  "{length} 次工具": "{length} tool calls",
  "已被你停止": "Stopped by you",
  "路由 {routeReason}": "Routed {routeReason}",
  "你好，我是 Joy": "Hi, I'm Joy",
  "把要做的事说清楚就行 —— 长任务会一步步来，需要你拍板时会停下来问。": "Just say what you want done — long tasks go step by step and stop to ask when a decision is yours.",
  "跳过记忆（{reason}）": "Skipped memory ({reason})",
  "新会话「{id}」—— 说第一句就有了": "New session “{id}” — say something and it starts",
  "「{title}」 · {messages} 条消息 · {id}": "“{title}” · {messages} messages · {id}",
  "{title} · 分支自「{forkedFrom}」": "{title} · branched from “{forkedFrom}”",
  "从这段对话开一个分支": "Branch from this conversation",
  "再开一个分支": "Branch again",
  "从「{title}」开一个分支": "Branch from “{title}”",
  "刚刚": "just now",
  "{length} 项": "{length} items",
  "看一眼": "Look",
  "技能 · {length} 个": "Skills · {length}",
  "＋ 添加技能": "+ Add skill",
  "还没有技能。技能是一份 SKILL.md：写上名字、用途和做法，Joy 会在相关消息出现时载入它。": "No skills yet. A skill is a SKILL.md: a name, its purpose and the method — Joy loads it when a related message shows up.",
  "「{name}」随 Joy 内置分发（JOY_SKILL_DIRS）—— 只能看，不能改": "“{name}” ships with Joy (JOY_SKILL_DIRS) — read-only",
  "内置": "Built in",
  "删除技能「{name}」": "Delete skill “{name}”",
  "只走 $名字 显式引用": "Only via $name",
  "定时 {schedule}": "Schedule {schedule}",
  "内置技能只读；自己加在 <home>/skills/ 下，下一轮就生效，不用重启。": "Built-in skills are read-only; add your own under <home>/skills/ — effective next turn, no restart.",
  "MCP 连接器 · {length} 个": "MCP connectors · {length}",
  "＋ 添加连接器": "+ Add connector",
  "还没有连接器。连上一个 MCP 服务器，它的工具就并进 Joy 的工具表（<服务器>_<工具>）。": "No connectors yet. Connect an MCP server and its tools join Joy's tool table (<server>_<tool>).",
  "无鉴权": "No auth",
  "浏览器 OAuth（joy mcp login）": "Browser OAuth (joy mcp login)",
  "「{name}」由系统内置配置提供（JOY_MCP_CONFIG）—— 只能看，不能改": "“{name}” comes from the built-in config (JOY_MCP_CONFIG) — read-only",
  "删除连接器「{name}」": "Delete connector “{name}”",
  "内置配置只读；改动落在 <home>/mcp.json，重启 Joy 后生效 —— 连接发生在启动时。": "Built-in config is read-only; edits go to <home>/mcp.json and apply after a restart — connections happen at startup.",
  "定时任务 · {length} 个": "Scheduled jobs · {length}",
  "＋ 添加任务": "+ Add job",
  "还没有定时任务。加一个：什么时候（cron，如 0 8 * * 1-5）、做什么（一句话）。到点它会在自己的会话里跑一轮，结果写进 outbox。": "No scheduled jobs yet. Add one: when (cron, e.g. 0 8 * * 1-5) and what (one sentence). Each firing runs a turn in its own session and writes the result to outbox.",
  "「{name}」写在 <home>/schedules.json": "“{name}” lives in <home>/schedules.json",
  "随技能": "From a skill",
  "删除定时任务「{name}」": "Delete job “{name}”",
  "下次 {nextRun}": "Next {nextRun}",
  "上次 {lastFiredMinute}": "Last {lastFiredMinute}",
  "还没跑过": "Never run",
  "上次被中断，重启时按「错过是否补跑」处理": "Interrupted last time; on restart it follows the catch-up rule",
  "投递 {delivery}": "Delivery {delivery}",
  "错过不补跑": "No catch-up",
  "到点执行的是 `joy schedule` 这个常驻进程 —— 它没在跑就不会触发（用系统 cron 驱动 `joy gather` 也一样）。输出落在 <home>/outbox/，不进聊天。": "The clock is `joy schedule` — if it is not running nothing fires (system cron driving `joy gather` works too). Output lands in <home>/outbox/, not in the chat.",
  "这个会话还没有记录。第一轮说完话，账本就会有第一行 —— 只追加，不改写。": "This session has no rows yet. After the first turn the ledger has its first line — append-only, never rewritten.",
  "账本 · {length} 轮": "Ledger · {length} turns",
  "只追加 · 可重建": "Append-only · rebuildable",
  "这一轮的工具输出只保留了摘要（回填而来，原文已不可重建）": "Tool output for this turn is summary only (backfilled; the original cannot be rebuilt)",
  "（空）": "(empty)",
  "{entries} 条": "{entries} rows",
  "已整理": "Consolidated",
  "折叠": "Folded",
  "这一轮没有可展开的条目。": "Nothing to expand in this turn.",
  "记忆库还是空的。它由后台整理写出来 —— 攒够几轮之后就会有自己的目录。": "The library is empty. The background worker writes it — after a few turns it will have a catalog of its own.",
  "目录 · {length} 页": "Catalog · {length} pages",
  "点左边的一页看看 —— 每页都带着它从哪几轮账本里来的。": "Pick a page on the left — each one carries the ledger turns it came from.",
  "这页没有记出处": "This page records no sources",
  "跳到账本里那一轮": "Jump to that turn in the ledger",
  "重新生成": "Regenerate",
  "换更强的模型后可以整页重跑（会先在后台重读来源轮次）": "A stronger model can rebuild the whole page (it re-reads the source turns in the background)",
  "还没有测量 —— 这个会话跑过一轮之后就有数了。": "Nothing measured yet — the numbers appear after this session runs a turn.",
  "上下文占用分层条": "Context usage",
  "灵魂 · 记忆 · 技能 · 工具 · 历史 · 摘要（hover 看明细）": "Soul · memory · skills · tools · history · summary (hover for detail)",
  "上下文占用": "Context usage",
  "现在压缩": "Compact now",
  "把较早的轮次折进摘要，保留最近两轮": "Fold older turns into the summary, keep the last two",
  "选择工作目录": "Choose a working directory",
  "选一个文件夹作为工作目录——它同时也是唯一的可写范围": "Pick a folder as the working directory — it is also the only writable scope",
  "工作目录：{path}（只有这里能写文件）": "Working directory: {path} (the only place files can be written)",
  "拿掉这个文件": "Remove this file",
  "{name} {hits} 次": "{name} × {hits}",
  "命中：{text}": "Hit: {text}",
  "还没有选会话 —— 先开一个会话，账本才有得看。": "No session selected — open one and the ledger has something to show.",
  "记忆库里没有「{slug}」这一页 —— 它可能在后台整理时被改名了，刷新目录看看。": "The library has no page “{slug}” — it may have been renamed during consolidation; refresh the catalog.",
  "账本里没有 {source} 对应的那一轮（可能已被整理进更早的摘要）。": "No ledger turn matches {source} (it may already be folded into an earlier summary).",
  "整页重生成走 CLI：joy memory rebuild --page ": "Whole-page rebuild is a CLI command: joy memory rebuild --page",
  "没有更早的轮次可折 —— 摘要已经跟上了。": "Nothing older to fold — the summary is caught up.",
  "已压缩：摘要现在覆盖 {coveredTurns} 轮（{summaryTokens} token）。": "Compacted: the summary now covers {coveredTurns} turns ({summaryTokens} tokens).",
  "停止": "Stop",
  "（那一轮已经结束了）": "(that turn already ended)",
  "上次中断的回合正在续跑（已完成的工具不重跑）": "The interrupted turn is resuming (finished tools are not re-run)",
  "取消任务「{title}」？取消后不会自动续跑。": "Cancel task “{title}”? It will not resume by itself.",
  "读目录中…": "Reading the folder…",
  "最近使用": "Recent",
  "这层没有子目录了。要选的就是当前显示的目录。": "No subfolders here. The folder shown is what you would pick.",
  "＋ 新建文件夹": "+ New folder",
  "在 {current} 下新建文件夹：": "New folder in {current}:",
  "隐藏点开头": "Hide dotfiles",
  "显示隐藏目录": "Show hidden folders",
  "选这个目录": "Use this folder",
  "工作目录没记到会话上（这一轮仍然用它）": "The working directory was not saved to the session (this turn still uses it)",
  "删除技能「{name}」？SKILL.md 会从 {folder} 移掉，下一轮起不再载入。": "Delete skill “{name}”? Its SKILL.md is removed from {folder} and stops loading next turn.",
  "删除 MCP 连接器「{name}」？重启 Joy 后不再连接它。": "Delete MCP connector “{name}”? Joy stops connecting to it after a restart.",
  "添加技能": "Add skill",
  "SKILL.md 内容": "SKILL.md content",
  "https://…/SKILL.md（和粘贴框二选一）": "https://…/SKILL.md (either this or the box)",
  "SKILL.md 网址": "SKILL.md URL",
  "安装": "Install",
  "粘贴一份 SKILL.md，或填一个网址。名字必须是小写 slug（字母、数字、连字符）—— 它会变成目录名；重名拒绝安装。": "Paste a SKILL.md or give a URL. The name must be a lowercase slug (letters, digits, hyphens) — it becomes the folder name, and a duplicate is refused.",
  "添加 MCP 连接器": "Add MCP connector",
  "名字，如 fs / notes": "Name, e.g. fs / notes",
  "名称": "Name",
  "https://host/mcp（远程 HTTP 服务器）": "https://host/mcp (remote HTTP server)",
  "远程 URL": "Remote URL",
  "npx（本地 stdio 服务器）": "npx (local stdio server)",
  "本地命令": "Local command",
  "参数，空格分隔，如 -y @modelcontextprotocol/server-filesystem /tmp": "Args, space-separated, e.g. -y @modelcontextprotocol/server-filesystem /tmp",
  "命令参数": "Command args",
  "鉴权环境变量名（可留空），如 NOTES_API_KEY": "Auth env var (optional), e.g. NOTES_API_KEY",
  "鉴权环境变量名": "Auth env var",
  "保存": "Save",
  "url 和 command 只能填一个。参数用空格分隔（路径里有空格就先编辑 mcp.json）。鉴权填环境变量的名字，不是密钥本身。保存后重启 Joy 生效 —— 连接发生在启动时。": "Fill either url or command, not both. Args are space-separated (a path with spaces? edit mcp.json first). Auth takes the env var's name, not the key itself. Edits apply after a restart — connections happen at startup.",
  "添加定时任务": "Add scheduled job",
  "morning-news（小写字母、数字、连字符）": "morning-news (lowercase letters, digits, hyphens)",
  "cron 表达式": "Cron expression",
  "到点让 Joy 做的事，比如：给我一份今天的科技简报，中文，三条以内": "What Joy should do, e.g. give me today's tech briefing, three bullets",
  "触发时做什么": "What to do when it fires",
  "投递方式": "Delivery",
  "outbox —— 写进 <home>/outbox/": "outbox — write into <home>/outbox/",
  "notify —— 写文件，同时推送通知": "notify — write the file and push a notification",
  "quiet —— 只记日志（高频任务）": "quiet — log only (high-frequency jobs)",
  "cron 五段：分 时 日 月 周。`0 8 * * 1-5` 是工作日八点，`30 19 * * *` 是每天十九点半。触发时会在自己的会话里跑一轮。": "Five fields: minute hour day month weekday. `0 8 * * 1-5` is 08:00 on weekdays, `30 19 * * *` is 19:30 daily. Each firing runs a turn in its own session.",
  "时间": "When",
  "做什么": "What",
  "投递": "Delivery",
  "删除定时任务「{name}」？schedules.json 里那一行会被移掉，joy schedule 下次触发前就读不到它。已经写出的 outbox 文件不动。": "Delete job “{name}”? Its line is removed from schedules.json, so `joy schedule` stops seeing it at the next tick. Outbox files already written stay.",
  "这一轮还在跑 —— 附件留到下一轮": "This turn is still running — the attachment waits for the next one",
  "{name} 超过 20MB 单图上限": "{name} exceeds the 20MB per-image limit",
  "（附件：{home}/{path}）": "(attachment: {home}/{path})",
  "已补充（本轮第 {queued} 句，排队）": "Added (message {queued} of this turn, queued)",
  "已补充，下一轮次生效": "Added; takes effect next turn",
  "已排队 —— 这一轮跑完就轮到它": "Queued — it runs when this turn ends",
  "思考中": "Thinking",
  "回复中": "Replying",
  "需要你授权": "Waiting for your approval",
  "已允许，并记住": "Allowed, and remembered",
  "已允许，本会话不再问": "Allowed for this session",
  "已允许（只这次）": "Allowed (this once)",
  "已拒绝": "Denied",
  "回答得太晚，那一轮已经不等了": "Answered too late — that turn stopped waiting",
  "只允许这次": "Allow once",
  "本会话总是允许": "Always allow this session",
  "这次启动内不再问它（不写盘，重启就忘）": "Do not ask again this run (not written to disk; forgotten on restart)",
  "记住这条": "Remember this",
  "写进 settings.json，下次启动也不再问": "Written to settings.json; not asked again after a restart",
  "拒绝": "Deny",
  "目标 · 第 {round}/{maxRounds} 轮 · {status} — {reason}": "Goal · round {round}/{maxRounds} · {status} — {reason}",
  "记住了 {newFacts} 条新事实": "{newFacts} new facts remembered",
  "工作流 {workflow}": "Workflow {workflow}",
  "工作流 {workflow} · {length} 步": "Workflow {workflow} · {length} steps",
  "（这一轮没有回复）": "(no reply this turn)",
  "这一轮没有收到结尾 —— 上面的回复可能被截断了": "This turn never sent its ending — the reply above may be truncated",
  "会话没建到服务端（首条消息会补上）": "The session was not created server-side (the first message will)",
  "会话的元数据没读到（工作目录沿用当前）": "Session metadata could not be read (keeping the current working directory)",
  "这一轮在别处跑 —— 可以补充它，也可以跑完再说": "This turn is running elsewhere — you can add to it, or wait for it",
  "发送": "Send",
  "排队": "Queue",
  "补充": "Add",
  "更早的消息已折叠 —— 用上面的「更早的」翻看": "Earlier messages are folded — use “Older” above",
  "页面里没有 #{id}": "The page has no #{id}",
  "清掉旧缓存并重新加载": "Clear the old cache and reload",
  "页面和脚本不是同一版": "The page and the script are different versions",
  "这个页面缺了 #{id} —— 浏览器缓存了旧版脚本（service worker 缓存优先），新页面配上了旧 bundle。": "This page is missing #{id} — the browser cached an old script (a service worker serves cache first), so a new page met an old bundle.",
  "点下面的按钮：会注销 service worker、清掉旧缓存，再取一份新版。": "Use the button below: it unregisters the service worker, drops the old cache and fetches a fresh copy.",
  "登录已过期，请重新登录": "Your login expired — sign in again",
  "新建一个会话": "Start a new session",
  "新建会话": "New session",
  "能力中心": "Capabilities",
  "技能：Joy 会的做法（SKILL.md）": "Skills: methods Joy knows (SKILL.md)",
  "技能": "Skills",
  "MCP 连接器：接进来的外部工具": "MCP connectors: external tools wired in",
  "MCP 连接器": "MCP connectors",
  "定时任务：到点在自己的会话里跑一轮": "Scheduled jobs: a turn in their own session, on time",
  "定时任务": "Scheduled jobs",
  "刷新": "Refresh",
  "切换界面语言": "Switch interface language",
  "拖动调整侧栏宽度（双击复位，方向键微调）": "Drag to resize the sidebar (double-click resets, arrow keys nudge)",
  "默认会话跟终端里的 joy 是同一条线": "The default session is the same thread as `joy` in a terminal",
  "进度": "Progress",
  "更早的 ↑": "Older ↑",
  "对话": "Conversation",
  "附上文件（20MB 以内；模型会自己去读）": "Attach files (20MB each; the model reads them itself)",
  "说点什么…（Enter 发送，Shift+Enter 换行）": "Say something… (Enter sends, Shift+Enter for a new line)",
  "消息": "Message",
  "这一轮跑完，再把这句话当成新的一轮发出去": "Send this as a new turn once the current one finishes",
  "跑完再说": "Queue for after",
  "面板": "Panels",
  "概览": "Overview",
  "记忆": "Memory",
  "定时": "Schedules",
  "收起": "Close",
  "记忆视图": "Memory views",
  "账本": "Ledger",
  "记忆库": "Library",
  "上下文": "Context",
  "检索「{q}」": "Search “{q}”",
  "工具 · {tool} {status}": "Tool · {tool} {status}",
  "好了": "ok",
  "出错": "error",
  "{n} 分钟前": "{n} min ago",
  "{month} 月 {day} 日": "{month}/{day}",
  "{time} 更新": "{time} updated",
  "依赖缺失：{list}": "Missing dependency: {list}",
  "鉴权：{env}（环境变量，值以 Bearer 发出）": "Auth: {env} (env var, sent as Bearer)",
  "「{name}」的定时来自技能 {skill} 的 SKILL.md —— 改那里，不在这里": "The schedule for “{name}” comes from skill {skill}'s SKILL.md — change it there, not here",
  "读不动：{error}": "Could not read: {error}",
  "建不了：{error}": "Could not create: {error}",
  "已排队 —— 前面还有 {n} 条": "Queued — {n} ahead",
  "回答没送出去：{error}": "The answer did not get through: {error}",
  "重试第 {attempt} 次（{reason}，{delay} 后）": "Retry {attempt} ({reason}, {delay})",
  "这一轮在别处跑了 {elapsed} —— 可以补充它，也可以跑完再说": "This turn has been running elsewhere for {elapsed} — you can add to it, or wait for it",
  " · 第 {round}/{max} 轮": " · round {round}/{max}",
};

let lang: Lang = "zh";
const warned = new Set<string>();

/**
 * Pick the language: an explicit choice wins, otherwise the browser decides.
 *
 * A Chinese browser gets Chinese; anything else gets English, because a reader whose
 * browser is in English is more likely reading English than not.
 */
export function initLang(): Lang {
  const saved = localStorage.getItem(STORAGE_KEY);
  if (saved === "zh" || saved === "en") {
    lang = saved;
  } else {
    const preferred = (navigator.language || "zh").toLowerCase();
    lang = preferred.startsWith("zh") ? "zh" : "en";
  }
  document.documentElement.lang = lang === "zh" ? "zh-CN" : "en";
  return lang;
}

export function getLang(): Lang {
  return lang;
}

/**
 * Switch language and reload.
 *
 * Every string is baked into a DOM node when it is painted, and a page can hold dozens of
 * nodes built by a dozen different functions. Re-rendering all of them correctly means
 * threading a "language changed" event through the whole file; a reload re-runs the one
 * code path that already knows how to paint everything.
 */
export function setLang(next: Lang): void {
  localStorage.setItem(STORAGE_KEY, next);
  lang = next;
  location.reload();
}

/**
 * Translate. `vars` fills `{name}` placeholders:
 *
 *     t("还有 {rest} 条", { rest: 3 })   →   "3 more"
 *
 * Placeholders are substituted **after** lookup, so word order can differ per language.
 */
export function t(zh: string, vars?: Record<string, string | number | bigint>): string {
  let text = lang === "en" ? DICT[zh] : undefined;
  if (text === undefined) {
    if (lang === "en" && !warned.has(zh)) {
      warned.add(zh);
      console.warn(`[i18n] no English for: ${zh}`);
    }
    text = zh;
  }
  if (vars === undefined) return text;
  return text.replace(/\{(\w+)\}/g, (whole, name: string) =>
    name in vars ? String(vars[name]) : whole,
  );
}

/** How many keys are translated — the language button shows it, and a test can assert it. */
export function translatedCount(): number {
  return Object.keys(DICT).length;
}

/**
 * Translate the static markup in index.html.
 *
 * Attributes, not just text: a placeholder and a `title` are as visible as a label, and
 * `aria-label` is what a screen reader says. `data-i18n` for text, `data-i18n-title` /
 * `data-i18n-placeholder` / `data-i18n-aria` for the rest.
 */
export function applyStatic(root: ParentNode = document): void {
  for (const node of root.querySelectorAll<HTMLElement>("[data-i18n]")) {
    const key = node.dataset.i18n;
    if (key !== undefined) node.textContent = t(key);
  }
  for (const [attr, dataset] of [
    ["title", "i18nTitle"],
    ["placeholder", "i18nPlaceholder"],
    ["aria-label", "i18nAria"],
  ] as const) {
    for (const node of root.querySelectorAll<HTMLElement>(`[data-${dataset.replace(/[A-Z]/g, (c) => `-${c.toLowerCase()}`)}]`)) {
      const key = (node.dataset as Record<string, string | undefined>)[dataset];
      if (key !== undefined) node.setAttribute(attr, t(key));
    }
  }
}
