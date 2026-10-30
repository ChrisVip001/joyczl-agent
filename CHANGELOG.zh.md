# 更新日志

[English](CHANGELOG.md) | 简体中文

## 0.7.0 — 向当前最优实现对齐（工作区版本；`v0.5.0` 是上一个已打 tag 的发布）

### 驾驶舱前端过期 = 拒绝启动

dist 是构建产物且被 gitignore，没有任何机制逼人重建 —— 出现过「源码更新
了一个多月、端出来的页面还是 9 月的」这种最隐蔽的坏（服务正常 200，用户
面对的却是一年前的 UI）。现在 `joy dashboard` 与 `joy hub` 启动时检查
新鲜度：**源码比 dist 新就硬错误**，错误信息里带重建命令。对齐守护锁同一
条哲学：配错就该起不来，而不是默默端出旧页面。发布场景（没有 joy-ts 源码
目录）不检查。

### 交互三件：工作目录、文件上传、模型快切

对照 codex / deepseek-harness / pi / hermes / openhuman 五家的交互，挑各自最优
拼成 joy 的最小实现：

- **会话工作目录**：`TurnStartParams.cwd`（codex 的逐轮覆盖语义）→
  `ToolCtx.cwd`。准入校验 fail-closed（hermes：必须是已存在的绝对路径）；
  exec 回退链 `args.cwd > cwd > 进程 cwd`、read_file/list_dir 相对路径按它
  解析（dsh 语义）；cwd 并入 exec 可写根（codex 的沙箱联动）；子代理继承
  父轮 cwd。REPL `/cwd` 与驾驶舱顶栏各一个入口。
- **文件上传**：`POST /api/upload`（octet-stream 单文件，避开 multipart
  解析器；20MB 上限）。落盘名完全由服务端生成（只取扩展名过白名单，用户
  输进不了路径——spill 同款纪律）；前端暂存区 → 发送时上传 → 消息注入绝对
  路径，模型用 read_file 自己读（pi 的「引用优先」）。图片不内嵌，见
  limitations。
- **模型快切**：头部模型 pill 变下拉（pi 的 /model 形态），与右侧模型面板
  共用同一条 `saveModel({id, makeActive})` 切换路径——持久配置，没有
  codex 那种「会话临时 vs 全局」之分（joy 的 profile 本来就是持久的）。

### 驾驶舱重设计：状态三态、工具卡片、数据即导航

参考 openhuman 的 AgentRunningStatus / ToolBodies / navConfig 与 dsh 的
对话渲染，vanilla TS 内消化（无框架、无新依赖）：

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
- **dispatch 先占坑再执行**（照 openhuman 的「dispatch 先推进 next_run」
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
- 三条策略抄自 openhuman 的 ProfileHost：**上限**、**LRU/idle 驱逐**、
  **in-use 永不驱逐**。驱逐判定不用时间戳猜并发 —— 用 `Arc::strong_count`：
  只有池自己持有的实例才可能被关，正在服务请求的天然 pin 住。
- 上限是**软的**：全都 pin 住时宁可超开也不把请求变成错误 —— 超开的是
  真实并发，不是闲置堆积。
- 附带收获：侦察还反向验证了 joy 的「每用户一个 home + 子进程」——
  openhuman 的 fleet 同样是 process-per-user，且自认不是生产级隔离边界。

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
  （token 在，费率随时能乘）。hermes 记了 `estimated_cost_usd` —— 等平台供 key
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
- **刻意不做**（五家都没有）：账号密码库、OAuth、per-user 配额强制；hermes
  式配对码准入（未鉴权端点 + 限速）延后到有真实需求时。

### M2 起步：驾驶舱 HTTP 面的鉴权门槛

- **`JOY_OPS_TOKEN`**：`/api/*` 的 Bearer token，常量时间比对；没有「本地请求
  免检」的旁路 —— 配了 token 就谁都要带，否则 token 只防得住外人。
- **非回环地址必须带 token**：`JOY_OPS_HOST` 绑到 `0.0.0.0` 或局域网 IP 而没设
  token，启动期直接拒绝 —— 配错就该起不来，而不是先当十分钟内网公开服务。
- token 刻意不进 `config/read` 的视图：部署面参数不该出现在模型看得见的载荷里。
- 驾驶舱页面首次 401 时问一次 token，存 localStorage 重试。

### 7x24 的地基：作业台账、守护锁、调度补跑

对着 codex / deepseek-harness / pi / hermes / nanobot 五家参考实现挖了一遍「长任务
怎么活过崩溃」，抄回来三件（它们的答案高度一致，正好钉死了此前的开放决策）：

- **作业台账落库**（迁移 0009）：后台作业的起跑/收尾写进 state.db，输出随写
  随落到 `<home>/jobs/<pid>-<id>.log`（1 MiB 封顶）。进程崩溃后，启动对账把
  还挂着的作业标成 `interrupted`、输出保留可回查 —— **不自动重跑**（五家一致：
  自动重跑是事故放大器）；`job_list` 能看到历史，`job_output` 能回读死掉的
  作业的输出，owner 围栏对历史同样生效；
- **守护锁**：同一个 `<home>` 只允许一个 app-server（`app-server.lock` + PID，
  学 codex 的 daemon；崩溃残骸由下一次启动自动收）。两个常驻进程各持一份
  内存态写同一个库是事故，不是功能；
- **调度补跑**：`joy schedule` 的上次触发表落盘（原子写），启动时回看自上次
  触发以来的分钟（封顶七天），错过就补跑**一次**并注明 —— 与 dsh 的
  schedule-resume 同口径，错过的触发不再被静默吞掉。

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
