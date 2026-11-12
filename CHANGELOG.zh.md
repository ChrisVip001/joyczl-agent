# 更新日志

[English](CHANGELOG.md) | 简体中文

## 未发布

### 工具数量是查出来的，不是记出来的

- `just check` 现在会跑 `scripts/check_tool_docs.py`（`check-tool-docs`）。内置工具的数量
  散在三处 —— 代码、`docs/internals/06-tools.md` §6.0（清单 + 「N 个常驻 + M 个门控」标题）、
  两份 README 的工具表 —— 三份副本必然漂移。脚本读「被测试钉住的注册表名单」和
  `app-server` 自己注册的那批，名字漏一个**或**数字对不上都会红。起因是发现文档写 22、
  代码里其实 28。

### 步数预算是预算，不是悬崖

- 用完 `JOY_MAX_ITERATIONS` 不再以「我还没做完就到了迭代上限」收场。**最后一步是收尾轮**：
  这一次调用把工具表清空，并告诉模型用三句话回答 —— 做完了什么、卡在哪、下一步需要什么。
  和 Claude Code 同形：它的交互式循环根本没有轮次上限（模型不调工具就结束）；我们保留上限，
  是为了让笔记本上的 agent 不至于花一整夜，但它限定的是成本，不是成功率。
- 默认值 `10` → `32`：10 小到一次诚实的探索（读文件、看 schema、重试一次）就能顶到天花板。
  `JOY_MAX_ITERATIONS` 仍然可以覆盖。
- 系统提示现在会说明本机**做不到**什么 —— `JOY_EXEC` 没开就没有 shell，`JOY_WRITE` 没开就
  不能写文件 —— 并附一句：做不到就直说并停下，不要用别的工具模拟。没有这句话时，模型分不清
  「工具不存在」和「我还没找到」，于是一次「清理日历」的请求变成了八轮读本仓库文档。

### 能力中心：定时任务

- 抽屉新增第五个页签**定时**：一行一个任务 —— cron、提示词、下次触发、上次触发、投递方式，
  数据来自 `<home>/schedules.json` 与技能 frontmatter 里的 `schedule:` 行。随技能来的任务
  在这里只读：要删就去改 `SKILL.md` 或删技能，行上写着这句话。
- 新增/删除文件任务会写 `<home>/schedules.json`（原子写、保留未知键与其它任务），走
  `POST /api/schedules/save` / `remove`。**解析不了的** `schedules.json` 会被拒绝写入，
  绝不覆盖。
- 「下次触发」、调度器与工具共用同一份实现：新增 `joyczl-schedule` crate（`cron` +
  `declarations`）。分开写会漂成「面板说八点，八点什么都没跑」。
- 新增三个工具 —— `schedule_job`、`list_schedules`、`cancel_schedule` —— 于是对话里说
  「帮我做一个定时任务」就能成。在它们之前，模型会去 `write_file` `<home>/schedules.json`，
  而那是每个工作区之外的文件：拒绝得没错，界面只好问你要工作目录，可那文件跟项目无关。

### 记忆中心：账本、记忆库、占用率

- 抽屉新增第四个页签**记忆**，三个只读视图。**账本**一轮一行，点开看这一轮的条目流；
  `折叠` 标的是走回填进来的轮次，那几轮的工具输出只以摘要形式存在。**记忆库**显示
  bundle 的目录和单页正文，`ledger:…` 的 chip 能跳回来源轮次。**上下文**是六段分层条
  （灵魂 / 记忆 / 技能 / 工具 / 历史 / 摘要），下面是已用、预算，以及到
  `JOY_CONTEXT_WARN_RATIO` 时的那句提示。
- 系统提示拆成**两半**：稳定前缀（灵魂 + 模型身份）与每轮的尾巴（日粒度时间、待办、
  记忆命中、技能目录、摘要）。Anthropic 在前缀末尾打 `cache_control` 断点，其他 wire
  靠各自的自动前缀缓存。用量账本开始记 `cache_read_input_tokens` 与
  `cache_creation_input_tokens`。
- 技能变成**可选**的：目录（名字 + 一句话，上限 `JOY_SKILL_INDEX_TOKENS`）常驻每轮提示，
  正文按需加载（`$名字` 引用、关键词命中，上限 `JOY_SKILL_BODY_TOKENS`，或者模型自己调
  新增的 `read_skill`）。命中会记下来，并在「技能」页汇总。
- `JOY_TOOL_POLICY=deferred` 只把常驻工具的 schema 放进请求，其余由新增的 `find_tools`
  在本会话里打开。受开关与审批约束的工具（`run_command`、`write_file`、`edit`、
  `delegate_task`）永远不藏。
- 工具结果清理改成**从旧到新**，最新的 `JOY_TOOL_RESULT_KEEP_RECENT` 条（默认 3）整条
  留着 —— 模型正在用它们推理。
- `JOY_SOUL_MODE`（`auto` / `thin` / `full`）决定内置脚手架包多少在 `SOUL.md` 外面；用户
  改过的灵魂文件永远优先于模式。
- REPL 的 `/compact [重点]` 与驾驶舱的**现在压缩**可以随时把较早轮次折进滚动摘要。
  `search_memory` 改为走和检索门同一条 hybrid 路径。
- 崩溃恢复有了年龄上限：`JOY_RESUME_MAX_AGE`（默认 60 分钟）在启动时把更旧的 `open`
  checkpoint 标成 `abandoned`，不再自动续跑 —— 续一个两天前的回合，回答的是「今天」
  已经变过的那个问题，还要为它花一次模型调用。设 `0` 回到旧行为；那一行仍留在账本里。

### 能力中心：侧栏里的技能与 MCP 连接器管理

- 驾驶舱侧栏新增「能力中心」：两个入口（技能 / MCP 连接器），各开右侧抽屉里
  的一个面板（抽屉现在是三个页签：概览 / 技能 / MCP）。计数就挂在入口上，
  「有几个」不用点进去就知道；聊天列照旧不被挤压，移动端顶栏也留着这两个入口。
- 技能：每个已装载技能一行（名字、版本、策略字段、缺失的依赖）。MCP 连接器：
  一行一个，带传输方式（HTTP / stdio）、目标和鉴权方式（`auth_env` 变量名 /
  浏览器 OAuth / 无）。
- **内置行置灰只读。** 来自 `JOY_SKILL_DIRS` 的技能、或来自新增的
  `JOY_MCP_CONFIG`（冒号分隔的额外 mcp.json 文件，与 `JOY_SKILL_DIRS`
  同一分法）的服务器，是随安装或机器分发的 —— 从一台机器的界面上删掉它，
  会让每一台共享它的机器一起失效。所有写操作一律拒绝并说明原因；用户自己的
  `<home>/skills/` 与 `<home>/mcp.json` 是唯一可写的地方。
- 人可以自己加：粘贴一份 SKILL.md 或给一个网址（校验和 `joy skill install`
  同一道门；技能下一轮即生效，不用重启）；加一个 MCP 连接器（一个名字 +
  恰好一种传输；`auth_env` 放变量名而不是密钥；落盘是原子的，重启后生效 ——
  连接发生在启动时）。内置包已占用的名字会被拒绝而不是被悄悄遮蔽：运行时
  先到的赢，用户配置里再写一份也不会生效。
- 新的 ops 端点（`/api/skills`、`/api/skills/install`、`/api/skills/remove`、
  `/api/mcp`、`/api/mcp/save`、`/api/mcp/remove`），和 `/api/fs` 一样是
  ops 本地视图 —— 协议本身没动。技能安装/抓取的逻辑收进 `joyczl-memory`
  （`install_from_url`、`fetch_text`、`raw_url`），CLI 与驾驶舱共用一道门；
  `joyczl-mcp` 的配置类型补了 `Serialize`（原子重写用）与 `config_paths_for`
  （两层分法）。
- **页面和 bundle 再也不会不是同一版了。** `index.html` 是网络优先、
  `main.js` 是缓存优先，service worker 的缓存可能把新页面配给旧 bundle；随后
  `must()` 在模块顶层抛错，整屏白掉 —— 连会话列表一起，只在控制台留一行
  没人看的报错。现在两道保险：构建给脚本 URL 打版本戳
  （`main.js?v=<build>`），缓存键每次构建都变，两半只能来自同一次构建；
  万一错配真的进了浏览器，`must()` 会画出可读的恢复页（「页面和脚本不是同一
  版」），按钮注销 service worker、清旧缓存再重载。冒烟脚本把版本戳钉住了。
- Service worker `VERSION` 升到 `joy-v17`。

### 历史展示剥掉模型专用的工具折叠

- 落库的助手回复带着 `\n[tools used: …]` 尾巴 —— `fold_tool_activity` 写给
  **模型**的（下一轮不忘记自己做过什么）。人也看到了它，于是疑惑「我的家目录
  列表怎么跑进聊天里了」。展示层现在把折叠摘掉（`markdown.ts` 的
  `stripToolFold`）：取最后一次出现的标记（工具输出可能引用它），且只在文本
  真的以折叠收尾括号结束时才剥。折叠原样留在数据库 —— 一份存储文本，两种画法。
  实时完成回复本来就是干净的。

### 侧栏会话列表优化

- 会话行改成**严格一行**，侧栏形态：标题在左（超长省略号），相对时间在右，没有别的
  —— 条数、会话 id 都进 tooltip。没有卡片框：悬停与当前选中是仅有的两处背景，
  选中行左侧一条 2px accent 窄条指路。↑/↓ 在行间移动焦点（Enter 激活 —— 按钮原生）。
- `sessionList` 不再往宿主 `<ul class="session-list">` 里嵌套 `ul.list` —— 直接
  返回 `<li>` 行，只有一层列表。概览页的「最近会话」共用同一批行（仍然是一份
  真值、两个表面）。
- service worker `VERSION` 升到 `joy-v14`：`main.js` 是缓存优先，前端改动不 bump
  版本就一直在跑旧包 —— 上一版「看起来什么都没变」正是这个原因。
- **侧栏边界可以拖动**：抓住边框左右拉，宽度记进 localStorage（180–480px，缺省
  232）。双击复位；抓手聚焦时 ←/→ 每次 24px 微调；骑在边框上的 7px 命中区在
  悬停/拖动时亮出 accent 线；指针捕获让拖动跟手。移动端侧栏横当顶栏，隐藏抓手。
- 被截断的标题支持**悬停跑马灯**：悬停 500ms 后标题横向滚动（40px/s，滚动期间
  摘掉省略号），滚到末尾停一拍、滚回开头，指针不离开就循环；移开立刻弹回。
  没被截断的标题不动；`prefers-reduced-motion` 用户完全不滚动（tooltip 仍有
  全文）。标题改成两层 span —— 外层在行边界裁掉，内层管省略号和平移。

### 驾驶舱一边流式一边渲染 Markdown（块级增量绘制）

- 对话气泡不再「流式期间摊着 `**`、传完才变成排版」：新增 `StreamMarkdown`
  （`markdown.ts`）让回复**一边到一边就画成 Markdown**，粒度是块 —— 每个动画帧
  用同一个 `parseMarkdown` 重新解析，但只重画仍在生长的最后一块；落定的块各留
  一个 DOM 节点不动，几千 token 的回复也不会每个 delta 都整体重排。断在半截的
  行内标记（`**`、未闭合围栏）下一帧自然长好。
- 解析器对半行前缀没有严格的前缀稳定性（流断在 `- ` 标记中间时，孤立的 `-`
  先被解析成段落，之后才并回列表），所以每帧对账落定块的序列化签名，从第一处
  不一致起作废重画 —— 正常情况全等，一次 O(回复长度) 的比较，与解析同阶。
- `finish()` 收尾画完最后一块；截断分支不再整体重绘。留下的唯一一次整体重绘是
  `completed.reply` —— 那是权威结果，delta 可能缺失。`node --test` 新增整行前缀
  的流式不变量测试（111 个测试）。

### 供应商目录接入 models.dev（`scripts/gen_catalog.py`）

- 加厂商不用改代码的两半凑齐了：运行时早有 `<home>/models-catalog.json`（补丁/
  追加，启动合并），这轮补上生成侧 —— `scripts/gen_catalog.py` 从
  [models.dev](https://models.dev)（MIT，OpenCode 团队维护）拉模型元数据，
  生成符合目录格式的条目：context 上限、vision、tool-call 取自上游；wire 与
  key_env 由内置 12 家的 joy 映射表决定（gemini→google、kimi→moonshotai、
  glm→zhipuai、opencode_go→opencode-go 等）。只有标准库。
- 脚本不替人拍板：默认模型取「支持工具调用里上下文最大的」，候选列表打在
  stderr 上提醒确认；`--out` 合并写盘（同 id 去重），不带则打印到 stdout。
  用法见 docs/configuration.md「模型目录」。

### Windows 钥匙串 + `SecretVault` 收口

- `joyczl-config` 在 Windows 上新增 Credential Manager 后端（`keyring` crate，
  CredWrite/CredRead）：密钥不再只能落明文 `secrets.json`。5 秒超时与「失败回落
  文件、如实报告落点」的纪律与 macOS/Linux 一致；两处的已上线路径（`security` /
  `secret-tool`）一字未动。已用 `cargo check --target x86_64-pc-windows-msvc`
  验证 Windows 分支可编译。
- 新增 `SecretVault` trait（`put`/`get`/`remove`，put 必须如实回报落点），
  `SecretStore` 是第一个实现；`apply_profile` / `resolve_key` 的签名改收
  `&mut dyn SecretVault` —— 服务器形态的 KMS 信封后端出现时只加实现、不改调用方。
  连接器与 MCP 的 token 文件暂未搬入：它们的轮换/锁语义（0600 + 临时文件改名 +
  跨进程锁）值得单独一轮，搬的时机是产品真出服务器版。
### 收口：目标不再是独立对象，只写在任务行上

- 删掉协议方法 `goal/set` 与 `goal/view`、类型 `GoalSetParams` / `GoalSetResponse`、HTTP 路由
  `POST /api/goal`，以及 `Server.goals` 这个「runner 的镜像」。真值一直在
  `agent_tasks.goal_condition` 上（0.14 起），镜像只是启动时从行里恢复、再由几处代码手工
  保持同步的第二份记录 —— 两个真相迟早会分叉。
- 循环改读任务行：条件读 `task.goal_condition`，轮次就是 `round_no`（写在同一行），终止态由
  `TaskStatus` 承担（`failed` + 理由），不再有第三份状态。`goal.rs` 只剩判断器。
- 人的入口收成一个：`/goal <条件>`（没有任务就建一条、有就改它；`/goal clear` 只摘掉条件，
  行留下当记录）。协议里因此**没有**设目标的入口 —— 程序侧要设就用 `task/create {goalCondition}`。
- 驾驶舱的任务卡不再读 `DashboardData.goal`：它本来就在读 `data.task`（同一行），渲染里那段
  「目标」是重复建模，一并删掉。
- 测试：`a_goal_is_the_task_row_and_nothing_else` 直接断言行（设 / 改同一行 / 清掉条件 /
  空会话不建行）；循环测试改为经 `set_task_goal` 起跑；超轮次的断言从「内存里还有目标」改成
  「行还在、`status = failed`、条件没被清掉」。

### 清理：三个没人读的开关

- `experimental`（`JOY_EXPERIMENTAL`）从 `Settings` / `SettingsView` / `SettingsPatch` 删除：
  它自始至终没有任何消费者，却在驾驶舱概览里显示成「实验 · experimental」。
- `apple_calendar` / `google_calendar` 两个 settings 字段同样删除：日程工具一直直接读
  `JOY_APPLE_CALENDAR` / `JOY_GOOGLE_CALENDAR`（`tools/src/calendar.rs`），settings 那份是
  `config/write` 能改、改了不生效的死字段。**协议变更**：`SettingsView` / `SettingsPatch`
  各少三个字段（TS / JSON Schema / Python SDK 生成物已重算）。
- 驾驶舱概览里的「实验」一行随之消失 —— 它显示的三位都不起作用。

### 清理：驾驶舱里没有页面的绘制代码

- 删 `renderUsage` / `renderApprovals` / `renderSchedules`，以及 `api.ts` 的
  `fetchUsage` / `fetchApprovals` / `fetchSchedules` / `fetchTasks` / `fetchModelProfiles` /
  `fetchModelRoutes` / `saveModel` / `saveModelRoutes` / `removeModel`：配置面板撤出驾驶舱之后
  它们没有调用者，只是被 esbuild tree-shake 掉的死源码。HTTP 端点本身保留（CLI / hub 仍在用）。

### 文档

- `joy --help` 补上 `joy bench`：它早就在（长任务基准，记录成功率与中断续跑率，不拦发版），
  只是从没写进用法。

## 0.16.0 — 一个会话一次只跑一轮（会话门 + 运行中补充）

### 会话门

- `turn/start` 先认领会话：同一 `session_id` 已经有回合在跑，就返回 `SESSION_BUSY`（-32003），
  错误里带着占用者的 `turnId` —— 「等它」还是「补一句」，都由这一个 id 决定。
- 认领发生在**任何副作用之前**：被拒的回合不会把任务翻成 `running`，也不会留下 checkpoint。
- 续跑同样过门：同一会话的两个 checkpoint 不会并肩恢复；抢不到的那个保持 `open`，留到下次启动。
- 会话门没有独立的数据结构 —— 它是 `turns` 表（`turn_id → 回合`）上的一条不变量，没有第二份状态可以漂移。

### 运行中补充（steering）

- 新方法 `turn/steer { sessionId, message }`：把一句补进**正在跑的**那一轮，在下一个轮次边界进入工作记忆。
  已经跑完的工具不重跑 —— 这正是它和「取消重来」的分界线。
- 三种情况都明说：会话空闲 → `INVALID_PARAMS` 并指向 `turn/start`（**不**静默开新一轮）；
  同一轮补第二句 → `queued: 2`；补的那句一定落库。
- `chat_log`：被补进来的句子是**独立的 user 行**（`meta.steer`），转录按发生的位置显示；
  `session_history` 把连续的 user 行合回一个问题，模型看到的仍是一问一答。
- 新通知 `turnSteered`：同一回合的其它客户端（第二台设备、REPL）当场知道有人补了一句。
- 驾驶舱：回合在跑时按发送 = 补一句；带附件的发送会明说「附件留到下一轮」（那一次的请求已经发出去了）。

### 排队下一轮（follow-up）

- 新方法 `turn/follow-up { sessionId, message }`：回合还在跑时排一句「这轮结束再做」。和 `steer`
  的分界线是**归属**：steer 属于这一轮，follow-up 属于下一轮。
- 队列是**一张表**（migration 0019 `session_queue`），不是进程里的 Vec：排进去的句子是给人的承诺，
  重启不该悄悄消失。没有状态列 —— 行存在 = 还没交给回合（claim-then-run），跑过之后记录在 `chat_log`。
- 一次只放一条：回合结束时取队头，会话门因此不会被队列自己撞上。
- 会话空闲时调用 → `INVALID_PARAMS` 并指向 `turn/start`，「等这轮结束」不该被解释成「现在就开一轮」；
  排队成功回 `queued`（你在第几位），写库失败明说没排上。

### 已知

- 会话门是**进程内**的：两个进程各开一个 app-server 时仍然互不知情（`turn/steer` 也只能找到本进程的回合）。
- follow-up 的通知仍然跟着「一条 SSE 流 = 一轮」：排队的句子要等下一轮开始才在流里出现。

## 0.15.0 — 会话有了自己的一行（会话元数据）

### 会话不再是标签

- migration 0018：新增 `sessions`（`id` / `title` / `cwd` / `created_at` / `last_at`），
  旧会话由 `chat_log` 反填。**只加表、只加列**，不动任何现有数据。
- `session/new` 真的落库：空会话立刻进列表，可以带名字、可以绑工作目录。`title` 留空时
  读取侧仍然回退「第一条用户消息」，与 0.14 之前看到的完全一样。
- 新方法 `session/get`、`session/update`（改名 / 改目录；`null` = 不动，`""` = 清空）。
  两个都和 `task/*` 一样**只给人调**：模型没有给自己会话改名或换目录的权力。
- 新方法 `turn/list`：现在在跑哪几轮（可按会话过滤）。`turns` 每条多了 `session_id` 与
  开始时间 —— 只按 `turn_id` 记的表回答不了「这个会话正在跑吗」。
- 写消息就会把会话行建出来，所以 REPL / 网关 / 定时任务产生的会话也都在列表里，
  不会再有「有对话、却没人列得出来」。

### 工作目录跟着会话走

- `turn/start` 不带 `cwd` 时取 `sessions.cwd`（优先级：本轮指定 → 任务自己的工作目录 →
  会话记着的）。目录后来被删掉，就当作「没有可写范围」并打一行提示，**绝不悄悄退回进程 cwd**。
- 可写范围仍然是这条目录本身：`JOY_EXEC_WRITABLE_ROOTS` 是额外根，home 不算。

### 修好的

- `just check` 的 eval 配方：用例按相对路径加载，之前 `cd joy-rs` 之后再跑必然
  `No such file or directory`。
- Python SDK 补齐 0.14 漏掉的任务类型导出，以及本次三个新方法的响应配对。

### 已知

- 工作会话 / 团队共享空间（成员、角色、接管、个人会话迁入）**挂起**，设计留在
  `docs/planning/user-center.zh.md`；本轮没有任何多用户代码。
- 同一会话的两个并发 turn 仍然没有串行保护（0.16 的会话门）。

## 0.14.0 — 完成是量出来的（任务实体版本）

### 任务自己站着

- migration 0017 重建 `agent_tasks`：`session_id` 可空（任务可以不属于任何会话），去掉
  「一会话一活跃任务」的部分唯一索引，行上新增验收条件、预算、已花费、自己的工作目录、
  上次停留原因。**故意是破坏性迁移**：旧行不迁移。
- 周围四张表各司其职：`task_runs`（一次执行一行 ——「执行 3 次」是数出来的）、
  `task_steps`（一次工具调用一行，幂等键 `<turn_id>:<seq>`）、`task_events`（人读的流水）、
  `task_artifacts`（真的存在的文件）。`turn_checkpoints` 加 `run_id`，`chat_log` 加 `task_id`。
- 任务方法多了 `task/get`、`task/runs`、`task/artifacts`（和原来一样：只有人能调）。

### 「完成」由事实决定

- `verify.rs`：三条确定性检查 —— `fileExists`、`fileContains`（纯子串，永远不当正则：
  一个写坏的模式不该卡住一轮）、`toolsOk`（数步骤台账，不数模型的自述）。每轮先量再问
  判断器，**验收不过就不叫判断器** —— 不能请模型去确认一个已经假的事实。
- `observe.rs`：把工具的自述和文件系统对上。「说写了、文件不在」记一条矛盾，这是唯一能
  拦住「假完成」的信号。路径收敛复用写入工具的那一套（`inside_workspace` 提成 public，
  `contained` 改为调用它）—— 一套实现，没有第二个说法；没选目录就不做路径检查，绝不退回 home。
- 新状态 `needs_attention`：**故意非终态** —— 机器量到了不该由它拍板的事，人看一眼（继续或
  取消）。预算超了也停在这里，并点名是哪一维超了。
- 任务级预算（时长 / Token / 工具调用）每轮判一次，在下一次模型调用花钱之前。
- 崩溃安全：停在 `running` 的步骤意思是「已发、结果没落盘」。启动时转成 `waiting`，
  **绝不重放** —— 可能已经发生的写入，由人决定。

### 驾驶舱

- 任务卡多三块：验收清单（逐条勾/叉，未通过旁边给实测结果）、交付物（文件 + 真实大小）、
  三条用量条（只有设了上限的维度画条；80% 转琥珀、100% 转红）。脚注写「执行 N 次 ·
  上次停在…」。`needs_attention` 有专属的警示标签。`sw.js` 升到 `joy-v5`。

### eval 与门禁

- `evals/deterministic/task-acceptance.jsonl`：假完成、超预算、真文件通过 —— 全离线、可重复。
  eval harness 现在能在用例的回合之前建任务，并断言它的终态、逐条验收结论与交付物数量。
- `smoke-resume.sh` 预置一个任务 + 执行 + 一个「已发未落盘」的步骤，断言新进程把它转成
  `waiting` 并如实收尾任务。
- `smoke-dashboard.sh` 端到端检查新字段（顺带确认同一会话的第二个任务现在是建得成而不是被拒）；
  `smoke.sh` 的 hub 段改成挑空闲端口，不再写死 7801。
- `joy bench` + `evals/long/tasks.jsonl`：10 个长任务基准（多轮写入、跨轮 `toolsOk`、写后精确改写、
  嵌套目录、以及 3 个中断恢复），分开统计**任务成功率**与**中断恢复率**（打断 → 进程真的重启 →
  下一轮收尾）。**只记录、不拦发版**；报告另存 `eval_report_long.json`，不覆盖发版闸门的结论。
  基准第一次跑就抓到两个真缺陷，都已修并各留一条回归用例：多轮步骤幂等键相撞（第二轮的写入被
  静默吞掉，`toolsOk` 永远只数到一次）；打断后任务循环继续烧轮次（用户的停止只在轮次用尽时生效）。
- eval 场景新增 `{"gate": …}` 内联应答（多轮任务每一轮都会重新过门）与 `restart: true`（换一个
  进程跑下一轮），长任务基准因此能真的验证重启续跑，而不是假设它成立。

### 执行计划（P1，`JOY_PLAN=1`）

- 长任务可以在第一轮之前多问一次模型，把计划写成 3–6 步落进 `task_steps`（`kind = "plan"`，
  `run_id` 为空 —— 计划比某一次执行活得久），并以纯文本附在本轮消息里。验收没过允许改一次
  计划（最多两版）；计划**永不携带验收条件**，模型不能靠重新计划把人的标准放松。
  开关默认关：多一次模型调用、又改变 prompt，属于由人按下的能力开关。
- 计划器坏了不影响任务：拿不到合法 JSON 就打印一行、照常跑这一轮。
- 抽出 `joyczl-agent` crate：验收（`verify`）、观察（`observe`）、计划（`plan`）与全仓唯一的
  路径包含规则（`paths::inside_workspace`）从 app-server 移出；写入工具改为 re-export 同一条
  规则（依赖方向 `tools → agent`，无环）。这个 crate 不碰 JSON-RPC、不持有 `Server`，事实判定
  能单独跑测试。

## 0.13.0 — 有人敲门（工作区版本）

### 入站邮件：反方向的那道门

- 手写 IMAP 接收器（LOGIN / SELECT / UID SEARCH / UID FETCH，993 隐式 TLS、143
  STARTTLS、其它端口明文）——和 SMTP 同一个理由：要四个命令，不要一个邮件全家桶。
  取信一律 `BODY.PEEK[]`：Joy 看一眼不等于替别人把信标成已读。
- 台账（migration 0015）：`connector_inbox` 按 (connector, mailbox, uid) 去重，
  `connector_cursors` 单独存水位。**首次运行只记水位、不拉历史** —— 收件箱里那几百
  封旧信不是「新来的」。正文只存解出来的纯文本（text/plain 优先，text/html 去标签
  兜底），附件不落盘：收进来的是「有人给你写了什么」，不是「有人给你发了什么文件」。
- 处理方式是人写在配置里的：`imap.mode: "notify"`（默认）只记录 + 通知，驾驶舱
  概览的「收件箱」卡片点「处理这些邮件」（`inbox/summarize`）才是人说「现在处理」；
  `"turn"` 是常设授权，模型不自己决定。轮询间隔有 30 秒下限，`mode` 写错启动即失败。
- 驾驶舱概览新增收件箱卡片（未读计数 + 最新五封），REPL 在轮次之间打一行
  「收件箱新增 N 封」，内容仍留给面板。
- `smoke-connector.sh` 加第 6 段：真二进制 → 轮询器 → 脚本化 IMAP → `state.db` →
  `inbox/list`，两次启动跨过水位（旧信不重复、新信才进来）。

### 飞书用户身份：以谁的名义发

- 应用身份说「这个应用写给你」，用户 token 说「**我**写给你」。`feishu_oauth` 走
  授权码流程（无 PKCE）、本地 `127.0.0.1:41766/callback` 接 `code`、token 落
  `<home>/connector-auth/feishu.json`（0600），用前按过期时间续期。
- `joy connector feishu login | logout | status`；`connectors.json` 里
  `"as": "user"` 才会以这个身份发，默认 `app` —— 身份是人的决定，拼错启动即报错。

### 业务形态（M5）：把 Joy 接进别人的流程

- `POST /api/run`：一轮进、一段 JSON 出（`turnId` / `reply` / `model` /
  `provider` / `routeReason`），body 就是 `TurnStartParams`。人看驾驶舱走 SSE，
  CI / cron / 另一个 agent 用这个。
- 入站 webhook（`<home>/webhooks.json`）：没文件 = 没这扇门（404）；`X-Joy-Secret`
  常量时间比较、密钥读不到即拒；未知名字与 `enabled: false` 同样是 404；`{{payload}}`
  换成请求体原文。这一轮仍是普通回合 —— 键盘前没人，破坏性工具会超时被拒。
- 档位可以由任务声明：`task/create` 与 `turn/start` 接受 `tier`
  （`agent_tasks.tier`，migration 0016），**声明优先于 `fixed` 模式**，拼错当场拒绝；
  新增 `model/routes` / `model/routes/save`，驾驶舱「模型」面板多了自动开关与三行
  「档位 ↔ 配置」。

### webhook 二期：陌生人的门需要的东西

- `signature: true`：必须带 `X-Joy-Timestamp` 与
  `X-Joy-Signature` = `hex(HMAC-SHA256(secret, "<timestamp>.<body>"))`，与本地时钟
  相差超过 ±300 秒 → 401，同一签名只收一次（重放 409；重试要换新时间戳）。格式与
  `openssl dgst -sha256 -hmac` 对过金标量，另一侧不用为它装任何东西。
- `limit: N`：每个 hook 每分钟 N 次，只计**通过鉴权**的请求（乱试不烧额度），
  超了 429 + `Retry-After`；`limit: 0` 加载即报错。
- `callback: "https://…"`：调用方立刻拿到 202，回合在后台跑完再 POST 结果
  （三次投递、1s/2s 间隔，没有队列）。
- 新 `smoke-business.sh`（任务 API / 三种 webhook 模式 / 档位）与
  `fake_callback_server.py`；`fake_sse_server.py` 支持多轮。

## 0.12.0 — 伸出去的那只手（工作区版本）

### 连接器：草稿之外的唯一例外

- 新增 `joyczl-connector` crate：email（SMTP，465 隐式 TLS / 587 STARTTLS）与
  飞书（应用身份 tenant_access_token，进程内缓存两小时）。不引 SMTP 全家桶、
  不引 native-tls —— 只写需要的那些行。
- **三道栅栏，缺一不可**：没有 `connectors.json` 就连工具名都不注册；配好也默认
  `live: false` 只写 outbox；每次实际发送弹一次审批卡（收件人 + 正文摘要）。
  白名单留空 = 谁都不许，失败关闭。
- 凭据走既有 `SecretStore`（钥匙串优先、0600 明文回退并如实报告）；
  `connectors.json` 只放意图。拼错的字段名直接启动失败，不静默变成「谁都不许」。
- `joy connector list / email / feishu`：人的直接路径，与模型共用同一条白名单与
  同一段发送代码（终端发送不弹卡 —— 敲命令的人就是批准人）。
- 驾驶舱新增「连接器」面板：哪个连接器、能不能真发、白名单多宽；开关仍在文件里，
  UI 只读。顺手修掉空任务时面板崩溃（`task: null` 当对象用）的 bug。
- 新冒烟 `smoke-connector.sh`：真二进制 + 假 SMTP + 假飞书端点，覆盖
  无配置 / 白名单外 / 草稿 / 真发（RFC 2047 主题、base64 正文、receive_id 形状）。

## 0.11.0 — 任务是有身份的（工作区版本）

### 长任务实体：目标循环降为执行器

- 目标此前只是进程内存里的一个字符串：重启即丢，也就无从谈起「暂停」。现在它是
  `agent_tasks` 表里的一行（migration 0014）：有 id、有名字、有状态、有轮次账。
  `goal/set` 成为兼容层——设目标即建任务，清目标即把任务降级为单回合；`/goal`
  和任务卡从此指向同一张表。
- **一会话一活跃任务**：部分唯一索引兜底，第二个 `task/create` 被拒并点名已有的
  那个——让人看见自己已经开始了什么，而不是堆一排幽灵。
- 状态机：pending / running / suspended / done / failed / cancelled。judge 判定
  达成 → done；做不到 / 轮次上限 → failed（诚实记为没做成，不伪造完成）；
  判断器故障 → suspended（停在原地，由人决定）。
- **暂停不是停止**：`task/suspend` 中断回合，但 checkpoint 标 `suspended` 而不是
  `abandoned`；`task/resume` 从断点续跑，已完成的工具绝不重跑。`task/cancel`
  才是停止——checkpoint 标 abandoned，永不自动续跑。
- 重启对账：`running` 的任务标 `suspended`（理由写明），回合若有 `open`
  checkpoint 则续跑并把它翻回 running。**人类按的暂停不会被自动恢复**。
- 事件总线多一类 `task` ping（`TaskUpdated` 通知蒸馏）；dashboard 任务卡显示
  状态、轮次、挂起理由，并带暂停 / 继续 / 取消三个开关——这是它们唯一存在的地方。
- 冒烟扩展：建任务 → 首屏可见 → 拒第二个 → 暂停 → 继续 → 取消，全链路真 HTTP。

## 0.10.0 — 崩了，接着来（工作区版本）

### 回合级 checkpoint：长任务引擎的地基

- 回合的工作记忆（模型的思考、工具调用与结果）过去只活在进程内存里——
  进程一死，整轮从头再来。现在每个**工具结果边界**落盘一次（migration
  0013 `turn_checkpoints`），新边界覆盖旧边界，只留最新。
- **启动对账即续跑**：新进程发现 `open` 的 checkpoint 就自动续跑——
  续的是用户亲口交代的回合，已完成的工具绝不重跑；崩溃前的 token 账
  和续跑后的加在一起。`turnResumed` 通知带上已完成轮数，从不静默。
- 续跑与fresh回合**共用同一条收尾**（`finalize_turn`）：trace、用量台账、
  对话日志、巩固——一条路，没有第二份会被忘掉的拷贝。
- 门控不变：续跑只回到 checkpoint 已批准的领地；新工具执行照走审批门
  （default-deny）。用户打断与回合内报错标 `abandoned`，不自动续跑——
  进程结束之后，`open` 只可能意味着崩溃。
- 事件总线长出三类敲门：`task_progress` / `task_resumed` /
  `task_suspended`——仍是轻量 ping，仍是单连接，复用 0.9.0 的合拍与重连。
- 端到端冒烟（`scripts/smoke-resume.sh`）：真起进程、预置崩溃残留、
  kill -9、重启——对账续跑的 stdout 帧与台账闭环逐条断言。

## 0.9.0 — 账号与每一块屏幕（工作区版本）

### hub：注册 / 登录

- `POST /api/auth/register|login` —— 全部 `/api/*` 里唯一在鉴权层之外的端点。
  密码用 PBKDF2-HMAC-SHA256（ring——经由 rustls 早就在依赖树里），加盐、
  常量时间验证；未知用户与错密码同判定、同耗时。
- users.json v2：`用户名 → { 密码凭证, devices: {设备名 → token} }`。
  **登录即配对**——从哪台设备登录就配哪台；同设备同密码拿同一个 token。
  旧的扁平 v1 文件装载时自动迁移。
- 座位：`JOY_HUB_MAX_OPEN` 兼任用户数上限。第一个用户必须从本机注册，或走
  `joy hub adduser`——全新的公网 hub 绝不让陌生人抢先认领。空 users.json
  现在允许启动。

### dashboard：任何一台 Joy 服务器的客户端

- 服务器地址可配置；登录卡（服务器 / 用户名 / 密码 / 设备名）在认证前取代
  整个视图。远程 401 打开登录卡（「登录已过期」）；本机同源部署保留一次性
  token 询问。
- 在真实浏览器里对活 hub 验证过：注册 → token → 应用启动并钉在该用户
  隔离的 home 上。

### 跨端同步

- `GET /api/events`：每个 app-server 一路 SSE 提示——回合、审批、记忆提炼、
  错误，各折成 {kind, turnId, sessionId}。提示是敲门；客户端重拉
  `/api/data`。500 毫秒合拍；断线永远重连。连接 pin 住用户的 app-server
  （暖着是刻意的）。
- `/api/*` 挂 `CorsLayer::permissive()`——客户端用请求头鉴权、从不带
  Cookie，跨源不加 CSRF 面。

### PWA + Tauri 桌面

- PWA：manifest、SVG 图标（品牌字标，含 maskable），service worker 缓存
  静态资源、永不碰 `/api`——手机上可安装。
- Tauri 2 壳（`desktop/src-tauri`，独立 workspace）：一个窗口、打包好的
  驾驶舱、登录卡指向任意服务器。`just desktop-dev` / `desktop-build`。

### 文档

- configuration（中英）：hub 章节围绕账号、配对、座位与部署矩阵重写；
  limitations（中英）：素账号不做什么（找回、限速、双因素），以及为什么
  同步是敲门不是投递。

## 0.8.0 — 能写、能看、能声明、能分发（工作区版本；`v0.5.0` 是上一个已打 tag 的发布）

### 文件写入工具：`write_file` / `edit`

- **默认关闭**；`JOY_WRITE=1` 开启——与 `JOY_EXEC` 同一个「能力由人类开关」
  的哲学。审批语义与 `run_command` 完全一致（`JOY_APPROVAL` 三态，走
  `ApprovalBroker`）。
- `edit` 要求 `old_string` **精确**匹配：命中 0 次或多次都是拒绝而非猜测；
  返回统一 diff。二进制文件拒绝（NUL 探测），越界路径与 `run_command` 同规。
- 拒绝以文本返回、模型可读可行动——「execute 永不 Err」的既有不变量不动。

### 图片输入，端到端

- provider 层新增 `ContentBlock::Image`；两种 wire 都会说（Anthropic image
  block；OpenAI content 数组 + data URI）。
- 无 `vision` 标志的厂商**明确报错**——绝不静默丢附件。
- `turn/start` 附件与驾驶舱上传按大小上限内嵌图片；token 估算按字节计。

### 模型目录

- 内置目录（12 家、上下文窗口等元数据）编译进二进制；
  `<home>/models-catalog.json` 按 id 覆盖合并。零网络、零新依赖，启动合并一次。
- `model/list` 与驾驶舱模型面板读合并后的目录。

### 声明式工作流

- triage / gather 的拓扑改为 **JSON DAG**
  （`joy-rs/joyczl-graph/src/workflows/definitions/`），装载期校验：静态边
  必须无环、路由必须是**注册的内置函数**（`key_router` / `needs_action`——
  白名单是封闭的）、每个节点必须有绑定的实现。改 JSON 也无法把控制流交给
  模型。JSON 由 `include_str!` 编译内嵌——工作流是行为，不是运行时配置。

### hooks：HTTP handler

- hook 条目现在接受 `command`（shell）**或** `url`（HTTP POST 携带事件
  载荷）。决策契约与 shell 退出码完全一致：HTTP 200 + 决策 JSON 的
  阻断/放行，等价于 exit 0/2。既有纯 `command` 配置不变。

### per-session 写段锁

- 每会话一把 tokio Mutex，只串行化 turn 的**写入段**（对话日志 append、
  提炼、压缩）；模型调用段不持锁——慢 turn 卡不住会话，`turn/interrupt`
  也进得来。SQLite 层的三守卫保留：它们防双写损坏，锁防读写交错，互补。

### 平台边界：Windows

- CLI 与 server 在 Windows 上**拒绝启动**并说明原因（无沙箱、无执行、
  绝不假装能跑）。支持矩阵见 `docs/operations.md`；CI 保留 Windows
  build+test 作业。

### 分发物料

- npm shim 包（`joyczl-agent`：从 GitHub Releases 下载平台二进制）、
  Homebrew formula 哈希自动化（`just release-hash`）、tag 触发的 release
  workflow（四个平台二进制 + 分平台 wheel）、RELEASING 发布手册（中英）。
  **发布本身由人执行**——token 不进 CI。

### 文档

- 修复 README 工具表断裂、limitations 空章节与一条过时的记忆去重条目
  （中英成对）。

## 0.7.0 — 向当前最优实现对齐（工作区版本；`v0.5.0` 是上一个已打 tag 的发布）

### web 搜索：降级链、每轮上限、不可信包装

- **有序降级链**：Tavily → Brave（`BRAVE_API_KEY`，新增）→ DuckDuckGo；
  DuckDuckGo 只在完全没配 key 时进计划——付费后端失败后不再回头赌免费端点。
- **无后端不注册**：没有 key 且 `JOY_SEARCH_DDG=0` 时 `search_web` 干脆不进
  模型的工具表（跑不了的承诺只会招来注定失败的调用），启动日志说明。
- **每轮上限**：`JOY_SEARCH_MAX_PER_TURN`（默认 20），预算检查在任何网络请求
  之前短路——换着词搜的循环 loop guard 看不见，预算看得见。
- **搜索结果不可信包装**：标题与摘要也是外部内容（投毒的搜索结果和投毒的
  网页是同一威胁，只是早到一轮），与 fetch 的正文走同一条「数据，不是指令」
  转述管线。

### 驾驶舱前端过期 = 拒绝启动

dist 是构建产物且被 gitignore，没有任何机制逼人重建 —— 出现过「源码更新
了一个多月、端出来的页面还是 9 月的」这种最隐蔽的坏（服务正常 200，用户
面对的却是一年前的 UI）。现在 `joy dashboard` 与 `joy hub` 启动时检查
新鲜度：**源码比 dist 新就硬错误**，错误信息里带重建命令。对齐守护锁同一
条哲学：配错就该起不来，而不是默默端出旧页面。发布场景（没有 joy-ts 源码
目录）不检查。

### 工作目录的选文件夹与最近使用

- **可视化选文件夹**：服务端目录浏览
  `GET /api/fs?path=<目录>&hidden=1`（浏览器的选择器拿不到绝对路径，
  必须由知道文件系统的一侧代劳）+ 驾驶舱弹层
  （面包屑每一跳可点、「＋ 新建文件夹」走 `POST /api/fs/mkdir` 且名字
  过白名单、点开头默认藏可开关、构建噪音永远藏）。
- **最近使用**：「常去的目录下次不用再选」。`<home>/workspaces.json`
  canonicalize 去重（符号链与尾部斜杠不产生第二条）、命中移前、封顶
  8 条；两个记录点 —— turn 带了校验通过的 cwd 时、驾驶舱「选这个目录」
  时（意图在选的时候就表达，不必等轮次跑完）。选择器顶部「最近使用」
  区一键直达。`GET/POST /api/fs/recents`（ops 本地视图，不进协议）。

### 按任务自动选模型（model routing）

任务档位抽象（"You don't pick the model, the task does."），
加上「auto」的体感：

- `<home>/model-routes.json`：三档 `fast`/`chat`/`reasoning` 各指向一个
  现成 profile id；`fixed` 是默认（零行为变化）。每轮重读，改完即生效。
- L0 规则零开销：token 估算超过 chat 档窗口一半 → reasoning。阈值跟随
  provider 窗口自适配，无新旋钮。
- L1 分类器（模糊带 ≥2k 未过半才跑）：问一次便宜小模型「这是要动脑的
  活吗？」，fail-open 回 L0。
- 解析失败回落 active 并在 `TurnMeta.routeReason` 说明 —— 路由绝不炸轮次。
- `JOY_MODEL_ROUTE=auto|fixed` 环境强制；REPL `/route` 三件事；驾驶舱
  页脚显示「路由 auto:reasoning → ds-r1」。
- 纪律：路由是服务端策略，模型没有换模型的工具；人类 active 永远优先。

### 交互三件：工作目录、文件上传、模型快切

把各家最优的交互拼成 joy 的最小实现：

- **会话工作目录**：`TurnStartParams.cwd`（逐轮覆盖语义）→
  `ToolCtx.cwd`。准入校验 fail-closed（必须是已存在的绝对路径）；
  exec 回退链 `args.cwd > cwd > 进程 cwd`、read_file/list_dir 相对路径按它
  解析；cwd 并入 exec 可写根；子代理继承
  父轮 cwd。REPL `/cwd` 与驾驶舱顶栏各一个入口。
- **文件上传**：`POST /api/upload`（octet-stream 单文件，避开 multipart
  解析器；20MB 上限）。落盘名完全由服务端生成（只取扩展名过白名单，用户
  输进不了路径——与 spill 同一条纪律）；前端暂存区 → 发送时上传 → 消息注入绝对
  路径，模型用 read_file 自己读（pi 的「引用优先」）。图片不内嵌，见
  limitations。
- **模型快切**：头部模型 pill 变下拉（pi 的 /model 形态），与右侧模型面板
  共用同一条 `saveModel({id, makeActive})` 切换路径——持久配置，没有
  「会话临时 vs 全局」之分（joy 的 profile 本来就是持久的）。

### 驾驶舱重设计：状态三态、工具卡片、数据即导航

运行状态三态、工具卡片、导航与对话渲染，vanilla TS 内消化（无框架、无新依赖）：

- **运行状态三态**：等待你（静止行）/ 思考中（shimmer + 秒表）/
  干活中（工具名 + 秒表）—— 区分「卡住了」和「在跑」。
- **工具卡片**：每个工具从 args 提取一句人话摘要（run_command 显示命令、
  read_file 显示路径、fetch_url 显示 URL），配实时秒表与定格结果；
  失败当场可见（此前只有 raw JSON chip）。
- **右侧面板 tab 化（数据即导航）**：概览 / 记忆 / **用量** / **审批** /
  **定时** / 模型 —— M2d 用量台账、M3b 审计、M4b 调度状态终于有 UI；
  按需取数（只刷当前 tab）。
- 新协议方法 `schedule/list`（无参，返回每任务的 last_fired/claimed/
  catch_up/delivery 快照）；ops 加 `GET /api/usage`、`/api/approvals`、
  `/api/schedules` 三条只读路由。
- 双层设计 token：材质层（:root 变量）+ 语义层，暗色只切一组变量；
  识别色保持暖橙。
- 顺手修一个必炸 bug：apiFetch 401 重试时误调自身（应为 fetch）——
  任何 401 都会爆栈。

### M4c：调度 run 账本 —— 「跑过哪些、各自结局」有处可查

- `<home>/schedule-runs.jsonl` 每次 run 一行 JSON：`ok`/`empty`/`error`
  三种结局都是一等公民 —— 之前 error 只活在 stderr，outbox 只在成功时
  写（quiet 连产出都没有），巡检没有统一入口。
- **刻意没做每作业迭代上限**：定时 turn 继承全局 `JOY_MAX_ITERATIONS`，
  子代理另有独立上限 —— 每作业覆盖需要动协议类型（TurnStartParams →
  生成物三端），而这个旋钮目前没有真实消费者。YAGNI，等有作业真的
  撞上限再说。

### M4b：调度升级 —— 状态进 SQLite，防重放与每作业投递

- 迁移 0012 `schedule_state`：去重（last_fired）、崩溃恢复（claimed）、
  每作业策略（catch_up/delivery）一张表全管 —— 原 schedules-state.json
  只做一次性导入的来源，不再是权威。
- **dispatch 先占坑再执行**（dispatch 先推进 next_run
  防重放）：触发前 `claim` 落盘，落定后 `confirm`。崩溃在两者之间时，
  重启的恢复逻辑显式接管 —— 按 `catch_up` 字段：晨报类（=1）补跑一次
  （至少一次），通知类（=0）放弃（至多一次）。**按作业类型给字段，
  不做全局二选一。**
- **每作业投递** `delivery`：`outbox`（默认）/ `notify`（文件 + 通知器，
  新事件 ScheduleRan）/ `quiet`（只落日志）。空回答或 `NO_REPLY` 跳过
  投递 —— 没内容的产出不值得打扰。
- 失败也落定：没有退避机制，不该每 20 秒重试一次失败任务。

### M4a：hub 资源池 —— 高密度的最小正确解

- 两个旋钮：`JOY_HUB_MAX_OPEN`（默认 32，同时最多开几个用户的
  app-server）与 `JOY_HUB_IDLE_SECS`（默认 900，闲置多久关；0 = 关闭此
  策略）。
- 三条策略：**上限**、**LRU/idle 驱逐**、
  **in-use 永不驱逐**。驱逐判定不用时间戳猜并发 —— 用 `Arc::strong_count`：
  只有池自己持有的实例才可能被关，正在服务请求的天然 pin 住。
- 上限是**软的**：全都 pin 住时宁可超开也不把请求变成错误 —— 超开的是
  真实并发，不是闲置堆积。
- 附带收获：侦察还反向验证了 joy 的「每用户一个 home + 子进程」——
  同形态的 fleet 也是 process-per-user，且自认不是生产级隔离边界。

### M3b：审批审计台账 —— 「谁批准了 rm -rf」有处可查

- 迁移 0011 `approval_log`：每次「问了才跑」落一行，outcome 四态 ——
  `approved`（人批了）/ `denied`（人拒了）/ `timeout`（没人答）/ `voided`
  （turn 结束作废，oneshot 发送端被丢的那一刻就知道）。
- **写入口唯一**：批准请求的等待 future 收尾处 —— 四个出口都从那里过，
  不存在「某条路径忘了记」。写失败不拦主路（台账是影子，因果由 oneshot
  通道保证）。
- 新协议方法 `approval/log`：最近 100 条，新的在前；读失败返回空列表并说明。
- 之前这条信息只活在内存与 stderr，turn 一结束就没了 —— 现在 hub 管理员
  事后能审计。

### M3a：通知器 —— 人不在屏幕前也该知道

三类「需要人看的事」推出进程：**审批请求**（去驾驶舱批准）、**后台作业收尾**
（completed/failed/killed/interrupted 都报）、**目标终局**（satisfied 也报 ——
自主循环停了就该让人知道）。

- 配置：`JOY_NOTIFY=1` + `<home>/notify.json`（`{"slack": webhook URL}`）。
  URL 是管理员亲手配的 —— **刻意不做 SSRF 护栏**：`fetch_url` 那套护栏防的是
  「模型指哪打哪」，管理员把通知指到内网自建端点（ntfy/Gotify）是合法用法。
- **旁路纪律**（与用量台账同一条）：fire-and-forget、10 秒超时、失败只往
  stderr 说一声，绝不拦 turn —— 通知系统坏了，主路照常。
- 实现是注入式 `Notifier` trait（与 ApprovalBroker/JobRegistry 同一条纪律），
  测试用捕获型 mock + 回环真发各覆盖一层。

### M2d：用量台账（只记账，不强制）

- 迁移 0010 `usage_ledger`：每 turn 一行（session/provider/model/输入/输出），
  `usage/summary` 协议方法给出 per-model 聚合 + 总数；写失败**不拦 turn**（记账
  是旁路）—— stderr 说一声就继续。
- **刻意没有成本列**：BYOK 模式下钱记在用户自己的厂商账单上，费率是部署方的事
  （token 在，费率随时能乘）。`estimated_cost_usd` 这类估算列 —— 等平台供 key
  （M4）再补，现在填的是拍脑袋数字。
- hub 模式下这就是 per-user 记账：每个用户一个 home 一个台账；跨用户汇总是
  管理面的活（M4）。

### M2：多用户单机（`joy hub`）—— 团队能用的形态

- **`joy hub adduser <名字> --root <目录>`**：生成一枚 token（getrandom，64
  hex，只打印这一次），写进 `<root>/users.json`（0600，临时文件 + rename）。
- **`joy hub`**：一个 ops 进程服务所有用户；Bearer token → 用户 → 该用户的
  home（`<root>/<用户名>/`），app-server 子进程**懒加载**（用到才起）。每个
  home 是完整的一套：state.db、models.json（**BYOK 直接复用 M1 的成果**）、
  jobs 台账、技能；守护锁本来就 per-home。
- **隔离是结构性的**：用户名装载期校验（只许小写字母数字连字符，`../` 进不
  来）；两个用户的 home 是不相交目录 —— 「A 的 key 不出现在 B 的路径」靠的
  是没有共享路径，不靠「记得别泄漏」。两条测试钉死这个结构。
- 冒烟加了 hub 段：无 token 401 / 错 token 401 / 对 token 200 / home 被建出。
- **刻意不做**（同类参考实现都没有）：账号密码库、OAuth、per-user 配额强制；
  配对码准入（未鉴权端点 + 限速）延后到有真实需求时。

### M2 起步：驾驶舱 HTTP 面的鉴权门槛

- **`JOY_OPS_TOKEN`**：`/api/*` 的 Bearer token，常量时间比对；没有「本地请求
  免检」的旁路 —— 配了 token 就谁都要带，否则 token 只防得住外人。
- **非回环地址必须带 token**：`JOY_OPS_HOST` 绑到 `0.0.0.0` 或局域网 IP 而没设
  token，启动期直接拒绝 —— 配错就该起不来，而不是先当十分钟内网公开服务。
- token 刻意不进 `config/read` 的视图：部署面参数不该出现在模型看得见的载荷里。
- 驾驶舱页面首次 401 时问一次 token，存 localStorage 重试。

### 7x24 的地基：作业台账、守护锁、调度补跑

对着多家参考实现挖了一遍「长任务怎么活过崩溃」，拿回三件（答案高度一致，
正好钉死了此前的开放决策）：

- **作业台账落库**（迁移 0009）：后台作业的起跑/收尾写进 state.db，输出随写
  随落到 `<home>/jobs/<pid>-<id>.log`（1 MiB 封顶）。进程崩溃后，启动对账把
  还挂着的作业标成 `interrupted`、输出保留可回查 —— **不自动重跑**（五家一致：
  自动重跑是事故放大器）；`job_list` 能看到历史，`job_output` 能回读死掉的
  作业的输出，owner 围栏对历史同样生效；
- **守护锁**：同一个 `<home>` 只允许一个 app-server（`app-server.lock` + PID，
  daemon 形态；崩溃残骸由下一次启动自动收）。两个常驻进程各持一份
  内存态写同一个库是事故，不是功能；
- **调度补跑**：`joy schedule` 的上次触发表落盘（原子写），启动时回看自上次
  触发以来的分钟（封顶七天），错过就补跑**一次**并注明 ——
  错过的触发不再被静默吞掉。

### 模型有了自己的眼睛：只读文件工具

`read_file` / `list_dir` **默认注册**。此前读文件的唯一路径是开 `JOY_EXEC` 再
`cat` —— 把无副作用的「看一眼」绑在执行的闸门上。现在：分页（2000 行 / 50KB，
行边界截断、绝不切半行）、行号、`[Use offset=N to continue.]` 续读协议、同一
会话重读同一段返回「没变」占位符、二进制文件明确拒绝。读取不走执行闸门，
`run_command` 原样不动。

`fetch_url` 抓网页正文（HTML→纯文本，50k 字符封顶）：SSRF 护栏（内网 /
metadata / 保留地址全拒，重定向**每一跳**重新解析重查），正文先过「数据不是
指令」的转述包装 —— 被注入的网页与被注入的子代理是同一类威胁，同一套防线。

### 行为变更：旧的「环境变量配 key」那条路拆掉了

以前配模型要 `export JOY_API_KEY=…`（或 `JOY_BASE_URL` / `JOY_MODEL` /
`JOY_SMALL_MODEL`，或写进 settings.json），而且 Joy 会**偷偷**按 `JOY_PROVIDER`
去读厂商默认变量兜底 —— 后果是「我明明没配过，它却因为我 shell 里有个变量就
跑起来了」，出问题时最难查。

现在**只有一条路**：`<home>/models.json` 里那套配置声明的凭据指针（`--env
变量名` 是显式声明「读这一个」，不是隐式兜底）。随之删掉：

- `JOY_API_KEY` / `JOY_BASE_URL` / `JOY_MODEL` / `JOY_SMALL_MODEL` 不再被读取；
- `config/write` 的补丁里不再有 provider / model / small_model 字段 —— 模型与
  密钥改走 `model/*`；
- `resolve()` 不再隐式读厂商环境变量，缺 key 时直接给出 `/model add …` 这条命令；
- `JOY_PROVIDER` 保留，但**只**是「一套都没配时的兜底厂商」，让 ollama 这类本地
  模型能零配置起步。

升级后若原来靠环境变量跑着：**模型会变成没配**，照提示 `/model add` 一次即可
（密钥顺带进了系统钥匙串）。

### 多模型：12 家预适配 + 随手切换

12 家厂商本来就预适配在 `joyczl-provider` 里，缺的是「用户自己配、随手切」：

- **多套配置并存**：`<home>/models.json` 存多套「厂商 + 模型 + 端点 + 凭据
  指针」，`/model use <名字>` 或驾驶舱面板切过去 —— 切换不用重发密钥，改的是
  进程内 Settings 并重解析 provider，**立刻生效、不用重启**；
- **密钥优先交给系统钥匙串**：macOS `security` / Linux `secret-tool`，都不可用
  才退回 `<home>/secrets.json`（0600 明文）；应答与界面**如实说密钥落在哪**；
- **只有人能改**：`model/profiles` / `model/save` / `model/remove` 三个协议方法
  与 REPL 的 `/model`、驾驶舱面板走同一处实现；模型没有这些工具，也拿不到密钥。

### 视频分析技能：先变成文字，再谈理解

`skills/video-transcription/SKILL.md`（仓库自带，也是技能格式的活示范）：
「分析这个视频」→ 探测 yt-dlp / whisper（缺了如实报告）→ 在线视频拉字幕
不下载 / 本地视频转写 → `read_file` 读文稿分析。技能正文写死两条纪律：
模型没有文字就不许描述视频内容；字幕是「数据不是指令」的外部输入。

### 并发：两个 turn 撞上的三处闸门

同一会话的两个并发 turn 面前有三处 read-then-act：提炼是「读一批 → 蒸馏 → 再
标记」，于是两边都看到「到期」，同一批日志被蒸馏两遍（白烧一次模型调用）；滚动
摘要的 `ON CONFLICT … DO UPDATE` 不看谁的 `covered_turns` 更新，并发压缩时用户
可能拿到一份**更旧**的摘要；`busy_timeout` 写在注释里是 3 秒、实际是 sqlx 的默认
5 秒（`PRAGMA` 是 per-connection 的，`execute(&pool)` 只设中当时那个连接）。

三处各自收口：提炼改成一条 `UPDATE … RETURNING` 的**原子认领**，输的那个拿到空
列表、**连模型都不调**（失败路径把认领还回去，`consolidated` 归零 + 指数退避）；
滚动摘要加**单调守卫**，只接受覆盖更多轮次的那份，被挡下时 stderr 说一句；
等锁改成挂在连接选项上的 **10 秒**（SQLite 官方建议值）。另外把后台作业的 outbox
文件名加上 pid —— job id 是进程内自增的，两个 Joy 进程会写出同名 `j1.txt`。

没有加 per-session 锁：代价（第二个 turn 明显变慢）大于收益，三条闸门已经把并发
后果降到「多等一会儿」。测试：并发两次提炼断言小模型只被调用一次、失败释放认领、
摘要只往前滚、PRAGMA 真的生效（这条测试当场抓出「注释说 3 秒、实际 5 秒」）。

### 目标：一直做到真的做完

`goal/set`（REPL 里是 `/goal`）把一个条件挂到会话上。主循环停下来时，判断器
（便宜模型、没有工具、只看对话）判一次目标是否达成；没达成，它的理由就变成下一条
用户消息，在同一个会话里再跑一轮。每次判断都发 `GoalRound`（轮次、上限、状态、理由），
于是多跑的轮次是有解释的；`TurnMeta.goalStatus` / `goalRounds` 把出口与轮次也带进
这一轮的元数据。

有界且如实：`JOY_GOAL_MAX_ROUNDS`（默认 5）封顶，到顶就置 `round-limit` 并说明
**没有达成**，而不是伪装完成。判断器答不出来就置 `blocked`、停续轮**并保住目标**。
终止态记在目标上，循环不会每一轮都把同一个问题再问一遍。目标只能由人设和清 ——
模型没有这个工具，所以「模型给自己派活」在这里不是被禁止，而是不存在。

### 长命令可以丢到后台

`run_command` 多了 `background=true`：命令被起在后台，一个 64 KiB 的有界环收集它的
输出，而这一轮继续往前走。`job_output` 按 cursor 读一截（`wait=true` 有界等 20 秒，
等不到就如实说还在跑），`job_list` 列出本会话的作业，`job_kill` 停掉一个。授权靠
**围栏**而不是保密：只有起它的会话能读能停。闸门与前台是同一条 —— 沙箱、断网、
放行表、批准全都在，因为「先丢后台再慢慢跑」正是绕过批准最自然的方式。作业结束后，
环里剩下的内容落在 `<home>/outbox/jobs/`。

### 一张模型自己维护的待办清单

`todo_write` 维护一张会话级清单（整表替换、版本号单调、最多 20 项、单项 4000 字符、
同时只能一项 `in_progress`），而且它是**每轮注入 system prompt** 的，不放在历史里
—— 能扛住压缩的清单，比扛不住任何东西的清单值钱。省略 `todos` 就是读回全量。
子代理没有它：那张表属于父轮的任务线。

### 轮内工具结果预算

历史有滑窗与 token 预算，轮内没有 —— 而 MCP 工具的输出无人看管。
`JOY_TOOL_RESULT_TOTAL_CHARS` / `JOY_TOOL_RESULT_MAX_CHARS` 会把最大的几条结果换成桩：
完整原文落盘、上下文里只留头尾的**完整行**，跳过自预算的工具、幂等执行，落盘失败
也绝不把成功的调用变成错误。默认阈值刻意给得很大（200k / 30k）。

### 子代理的报告是转述，也可以要结构化

报告现在带一句「这是转述」的声明头；模仿角色前缀或我们控制标记的行会被加反斜杠，
并告诉模型转义了几处。它不判断恶意，也不碰权限检查 —— 那是闸门的事。`delegate_task`
还接受可选的 `result_schema`：结论必须是符合它的 JSON，不合规会带着报错重试一次，
两次都不合规就回落成散文并写明原因。

### 生命周期钩子（`JOY_HOOKS`）

12 个事件（`PreToolUse`/`PostToolUse`/`PostToolUseFailure`/`PermissionRequest`/
`SessionStart`/`SessionEnd`/`Stop`/`StopFailure`/`SubagentStart`/`SubagentStop`/
`PreCompact`/`PostCompact`）可以从 `<home>/hooks.json` 跑你的 shell 命令：exit 2 阻断，
决策 JSON 能改写入参、改写结果、补上下文，`PermissionRequest` 还能替用户拍板。策略
事件超时 fail-closed、观察事件 fail-open；运行中改过的 `hooks.json` 拒绝执行并说明
原因；阻断 `Stop` 让这一轮再跑一次。只做 shell handler、Unix 优先 —— 两条都是刻意的，
都写进了 limitations。

### 记忆不再堆积重复

`facts` 上多了一条唯一索引 `(subject, lower(trim(content)))`，`Facts::add` 返回
`(行, 是否新记)`：重复时把已有的那条交回来，而不是再存一份。提炼只把真正新增的计入
账，`save_note` 会说一句「这条已经记过了」而不是假装又记了一次。迁移 0008 在建索引
之前先清掉历史重复 —— 建不起来的索引会让之后每次写入都报错。

### MCP：挂掉的服务器不再每轮白等一个超时

连接现在带断路器（连续 3 次失败 → 60 秒，按服务器隔离）：开着时调用直接返回说明而
不再敲门，冷到点后放一次探针，成功即清零。会塌成同名的工具（`a`+`b_c` 与
`a_b`+`c`）会加后缀去重并**打印改名**；`ToolRegistry::register` 遇到重名拒绝后到者，
而不是悄悄换掉实现。

### 用实测校准估算

`usage.input_tokens` 过去只是记下来、从不回喂。现在 loop 把**同一个请求**的估算与实测
一起返回，app-server 配对写进 `session_context`（迁移 0007），下一轮的预算按比值修正
（夹在 0.5–2.0，一次异常请求带不偏它）。

## 0.6.0 — 护栏、预算，以及一个离线的沙箱

### 配置在启动期校验

越界的值过去会静默变成默认值。现在 `Settings::validate` 在环境变量与
`settings.json` 合并之后跑一次，非法值直接终止并在消息里点名是哪个变量。
边界表（`joyczl-config::BOUNDS`）同时服务于启动路径与 `config/write`。

### 工具参数按 schema 校验

每个工具本来就声明了 `input_schema`；现在它在注册时被编译，并在 handler
之前强制执行。于是非法调用回来的是一句
`Error: 参数不符合 … 的 schema —— /subject：42 is not of type "string"`，
而不是各工具手写的提示；而且校验没过时 handler 根本没跑，不留半个副作用。

### 循环护栏

模型卡在同一个调用（连续三次完全相同）或在几个调用之间来回（`A,B,A,B`）时
现在会被告知，字节级重复的长结果会被换成引用桩。交替循环需要单独判定 ——
它会把「连续相同」的计数重置 —— 所以两种形状都抓。`TurnMeta.guard_hits`
让「这一轮卡过」在 trace 与驾驶舱里都看得见。

### 上下文按 token 做预算

`ProviderInfo.context_window` 加 tiktoken 估算决定什么时候压缩：轮数从此
是**上限**，token 才是闸门。provider 报「上下文超了」会被认出来
（`ProviderError::ContextOverflow`），这一轮会压缩后**重试一次**。
`JOY_CONTEXT_WINDOW` / `JOY_COMPACT_THRESHOLD` 可以调；校验会拒绝
`JOY_MAX_TOKENS >= JOY_CONTEXT_WINDOW` 这种答案放不下的配置。

### 限流与临时故障会重试，而且看得见

429/5xx/网络抖动会指数退避重试（单次等待 500ms 起、上限 8s，总预算 30s，
`JOY_LLM_RETRIES` 默认 2，`0` 关闭）。每次重试都发一条 `Retry` 通知并记进
`TurnMeta.retries`，于是那段等待是**有解释的**而不是莫名其妙的；REPL 打一行
小字，驾驶舱显示一枚标记。服务端给了 `Retry-After` 就按它等（受上限约束）。
不做 provider failover —— 那是另一个决定。

### 技能有了策略字段，记忆有了类别

技能现在可以声明 `allow-model-invocation: false`（「别不小心把我叫出来」）与
`dependencies: a, b`。声明了不隐式触发的、以及依赖缺失的技能，都不会因关键词重合
而载入；消息里的 `$技能名` 强制把正文放进去（并且会从模型看到的消息里剥掉）。
引用了不存在的东西是一句提示，不是错误。

事实带上了 `kind`（`user` / `feedback` / `project` / `reference` / `fact`），由提炼
模型给出、在写入口收敛，并通过 `memory/search`、`memory/list` 与协议里的 `Fact`
暴露出来。提炼失败现在按指数退避（1 分钟翻倍到 1 小时），而不是每轮重试同一批
坏行。

### 交互式批准（`JOY_APPROVAL=on-request`）

放行表没匹配上的命令，现在可以是「问一句」而不是直接拒绝。这一轮发
`approvalRequested`（工具、命令原文、为什么、时限）然后等；`approval/respond`
回答它；终端与驾驶舱都实现了回答那一侧。**沉默就是拒绝**：超时、答得太晚、
连接断了、根本没有界面，结局都是不跑。只有放行表那一关可以商量 —— 硬拒名单与
沙箱批不掉。「记住」把原样那条命令写进 `settings.json`，下次启动生效。

### 子代理（`JOY_DELEGATE`）

`delegate_task` 把一件自成一体的活交给子代理，在一个干净上下文里（空历史、不带
检索、不带技能）跑完，只把结论带回来。它拿到的是父轮工具**去掉 `delegate_task`**
的副本 —— 再派一层不是被拒绝，而是不可能；它也不能向你提问；答案的预算上限是
5 轮（硬上限 10）与 2048 token。结论后面附上它用过的工具（含失败的调用），父轮
因此能判断这份结论是不是建立在失败之上。默认关。

### 超长命令输出会留下来，不只是截断

超过 8000 字符的部分照旧不进模型看到的内容（上下文要保住），但完整原文会写进
`<home>/spill/<日期>/…` 并把路径报出来，随时可以回查。`spill/` 保留 7 天、启动
时打扫；写失败则退回纯截断。

### 行为变更：沙箱内的命令默认断网

`run_command` 现在默认**没有网络** —— seatbelt 加 `(deny network*)`，
bubblewrap 加 `--unshare-net`。需要下载东西的命令（比如会拉 crate 的
`cargo test`）必须设 `JOY_EXEC_NETWORK=1`。额外可写的目录用
`JOY_EXEC_WRITABLE_ROOTS` 放开（冒号分隔，每条必须是已存在的绝对目录），
构建缓存是典型用例。启动那一行会把两者都报出来。

## 0.5.0 — 本地、沙箱、定时

- 本地推理：`ollama` provider —— 不要 key、不要网络，对话不出这台机器。
  LM Studio / vLLM 用 `JOY_BASE_URL` + `JOY_MODEL` 指过去
- `joy mcp serve`：Joy 成为一台 MCP 服务器，暴露五个记忆工具，这台机器上的
  其他 agent 共享同一份事实（**只暴露记忆**，不暴露任何「替别人动手」的能力）
- 沙箱执行：`run_command` 挂在 `JOY_EXEC` 后面，默认关，三道闸门 ——
  不可配置的硬拒名单、放行表（空 = 全部拒绝）、macOS `sandbox-exec` /
  Linux `bubblewrap`（写权限限制在工作目录、Joy 的 home 与临时目录）。
  沙箱不可用就拒绝执行，绝不在沙箱之外跑
- 上下文压缩：被滑窗挤出去的轮次折进按会话的滚动摘要（存 state.db，只往前滚、
  不重算全史）；摘要模型罢工时退化为确定性摘录
- `joy schedule`：定时任务可声明在技能 frontmatter（`schedule: 0 8 * * 1-5`）
  或 `schedules.json` 里，五字段 cron，同一分钟最多触发一次，结果写进 outbox
- 混合检索（`JOY_EMBEDDINGS`，关键词与向量两条腿按名次 RRF 融合）+
  `joy memory reindex` 给开关打开之前的事实补向量
- consolidation 过滤临时陈述，不把它们当成事实存档
- `joy skill update`：按索引更新 —— 先校验、再暂存、备份、原子替换，从不降级
- 分发：Dockerfile（多阶段、非 root、装了 bubblewrap）与 Homebrew formula 模板
- `docs/limitations.zh.md`：已知边界清单（刻意的与未做的），每条都指出下一步
  该打开哪个文件

## 0.4.0 — 功能面补齐

- 协议：`turn/interrupt`（取消令牌 + 竞速收兵）、`config/write`
  （settings.json 持久化 + 热生效）、`model/list` 全部实现；
  `ToolStarted` 通知、`TurnMeta.usage` / `meta.interrupted` 入库
- CLI：终端 REPL（`joy` 裸跑）、`joy gather` 晨报、`joy mcp login`、
  `joy skill export/install/list`
- 工具：`search_web`、`create_event`/`list_events`（幂等 + ICS +
  Apple Calendar 可选同步）、`send_message`（outbox 草稿）、
  `manage_memory`、`create_skill`
- 记忆：Skills 过程记忆（渐进披露 + 关键词触发）、MEMORY.md 每轮镜像
- 观测：trace 落 `traces/<日期>.jsonl`、usage 账本 `usage.jsonl`
- 图：gather 晨报工作流（四路并行、只提议不行动、失败开放）
- MCP：浏览器 OAuth（发现 / 动态注册 / PKCE / 回调 / token 落盘 /
  刷新），stdio + HTTP 双传输
- 评测：`joyczl-eval`——确定性 eval（13 场景）+ release gate（`just
  check` 尾部）+ judge
- 文档：architecture / configuration / protocol / testing /
  operations / skills / CONTRIBUTING / SECURITY

## 0.3.0 — 驾驶舱与网关

- `joyczl-ops`：axum + SSE 驾驶舱后端；`@joy/dashboard` 前端
- 网关：Telegram、Discord（WebSocket + 心跳重连）、微信（三种加解密
  模式 + 4 秒竞速 + 客服消息）、飞书（长连接 + pbbp2 帧 + 分片重组）
- 会话：`session/*` 协议与历史翻页

## 0.2.0 — loop 与编排

- `joyczl-provider`：11 家厂商、Anthropic/OpenAI 双 wire format、SSE 流式
- `joyczl-loop`：observe → reason → act → repeat，工具错误文本化
- `joyczl-memory`：检索门（失败开放）+ consolidation
- `joyczl-tools`：内置工具注册表；`joyczl-graph`：波次 DAG 引擎 +
  triage 前门；`joyczl-mcp`：stdio + HTTP 传输

## 0.1.0 — 地基

- `joyczl-protocol`：单一事实来源 + TS/JSON Schema/pydantic 生成管线
- `joyczl-state`：SQLite + FTS5(trigram) + 迁移
- `joyczl-app-server`：JSON-RPC over stdio；`joy` CLI
