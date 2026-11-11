# 已知边界，以及要改从哪儿下手

[English](limitations.md) | 简体中文

这份清单里的每一条都是**已知**边界：要么是刻意的设计决定，要么是还没做的
工作。没有一条是「没想到的 bug」。每条都写明：边界是什么、为什么是这样、
想改的话先打开哪个文件。

读法约定：**刻意** = 这就是期望的行为，改动属于设计变更，先讨论；**未做** =
我们希望有，还没人做。

## 多模型与密钥存放

- **只有一条配置路**：`JOY_API_KEY` / `JOY_BASE_URL` / `JOY_MODEL` /
  `JOY_SMALL_MODEL` 都已不再读取，`config/write` 也不再接受模型字段 —— 改模型
  只能用 `model/*`（REPL 的 `/model`、驾驶舱面板）。`JOY_PROVIDER` 只剩「一套
  都没配时的兜底厂商」这一层意思。原来靠环境变量跑着的，升级后会变成「没配
  模型」，照提示 `/model add` 一次即可。
- **Windows 上没有钥匙串可用**：凭据管理器存得进、读不回明文，所以 Windows
  上（以及任何 `security` / `secret-tool` 不可用的环境）密钥会退回
  `<home>/secrets.json` —— **明文**，Unix 上权限 0600，Windows 上没有这一说。
  `model/save` 的应答会如实写 `file`，界面也照抄给用户看。
- **钥匙串命令有 5 秒超时**：`security` 在无人值守的会话里可能弹框或干等，
  超时即当失败并走文件兜底 —— 一次凭据操作不该冻住整个服务。
- **切换只影响当前进程**：改的是进程内 Settings 并重解析 provider；配置本身
  写在 `<home>/models.json`，下次启动仍然是你上次选的那套。
- **模型不能自己换模型**：`model/*` 只给人用，模型没有对应工具，也拿不到密钥。

## 通知与审计

- **通知是尽力而为**：发不出（网络断、端点 5xx）只往 stderr 说一声，**没有
  重试、没有投递账本** —— 五态 delivery ledger（pending/attempting/
  delivered/…）留给档位 B（作业结果能唤醒会话时）再引入，现在通知只是
  「顺便喊一声」，丢了就丢了。Slack 的 `{"text": …}` 格式也兼容 ntfy/Gotify
  这类自建端点；钉钉/企业微信的加签格式没做。

- **hub 账号是真的，但很素**：注册/登录发 per-device token（登录即配对），
  但**没有密码找回流程**（主人用 CLI 改 users.json）、**登录无限速/锁定**
  （错一次多等 200 毫秒，仅此而已）、**没有邮箱验证或双因素**、**没有邀请
  链接或 OAuth**——准入 = 座位上限 + 首用户回环规则。per-user 配额同样记账
  未设限（usage 数据在攒，强制留给计费）。
- **跨端同步是敲门，不是投递**：`/api/events` 的提示只说「有事发生」，
  客户端自己重拉所有要显示的东西。没有消息级增量协议、没有离线队列、没有
  推送通知（APNs/FCM）——手机在 PWA 打开时才知道活干完了。

## 常驻与守护

- **app-server 有守护锁**：同一个 `<home>` 同时只能跑一个 app-server
  （`<home>/app-server.lock`，含 PID；进程死了锁由下一次启动自动收掉）。
  两个常驻进程各持一份内存态（审批、目标、作业表）却写同一个库 —— 那是
  事故，不是功能，所以是硬错误而不是警告。REPL 与 `joy schedule` 不拿锁：
  它们是独立的单次进程。
- **调度会补跑错过的触发**：`joy schedule` 的上次触发表落盘在
  `<home>/schedules-state.json`；启动时对每个任务回看自上次触发以来的分钟
  （封顶七天），有错过的点就补跑**一次**并在提示里注明。停机三天不会补出
  三天的量。

## 执行（`run_command`、`JOY_EXEC`）

`read_file` / `list_dir` 默认开、不走执行闸门 —— 信任边界是**进程用户的操作系统
权限**：模型能读到该用户可读的任何文件（与同类只读工具一致）。不想让
模型看到的文件，放在该用户读不到的地方。`fetch_url` 与之对称：它的网络访问
**不经过** `run_command` 的沙箱（那是进程内直连），护栏是 SSRF 地址检查 + 外站
内容转述，不是沙箱；HTML→纯文本是手写转换器，复杂排版页的还原质量不如
readability 类库（真要高质量转换，走 run_command + curl 或 MCP 抓取服务）。

* **刻意** —— 沙箱内的命令默认**离线**（`JOY_EXEC_NETWORK=1` 才联网）；写权限
  限制在工作目录、Joy 的 home 与临时目录，外加 `JOY_EXEC_WRITABLE_ROOTS` 里
  列出的目录。要下载东西的命令得同时打开这两个开关。
  → `joy-rs/joyczl-tools/src/exec.rs`（`sandbox_argv`）
* **刻意** —— 写入工具（`write_file` / `edit`，`JOY_WRITE=1`）**没有进程沙箱**：
  写发生在进程内，所以硬闸是可写范围（本轮选的工作目录 + `JOY_EXEC_WRITABLE_ROOTS`，
  逐跳解析符号链接）。**没选工作目录就一处也写不了**；Joy 的 home 和进程 cwd 都不在
  范围里。范围外在审批之前就被拒绝——审批只可能放行一条本来就在范围内的写入。`require_str` 会 trim，所以写入工具的正文走不 trim 的取值
  ——尾部换行是数据，不是噪音。
  → `joy-rs/joyczl-tools/src/fs_write.rs`（`contained`、`raw_str`）
* **刻意** —— 落盘的命令输出只按**时间**清（`spill/` 保留 7 天，启动时打扫），
  不做容量配额。配额猜错了要么撑满磁盘、要么删掉你还想要的东西；按时间扫一遍
  是可预期的。`JOY_HOME` 里没有别的东西会轮转。
  → `joy-rs/joyczl-tools/src/exec.rs`（`prune_spill`）

* **刻意** —— 硬拒名单是**子串匹配**，变着花样的写法能绕过去。它是安全带不是
  证明：真正管用的是放行表与沙箱。
  → `joy-rs/joyczl-tools/src/exec.rs`（`HARD_DENY`）
* **刻意** —— 批准只有**档位**，没有按工具分类的粒度。`JOY_APPROVAL` 就是
  `never`（默认）与 `on-request` 两档；更细的版本（执行要问、MCP 不问）没做，
  因为目前只有 exec 有那个「问」的闸门 —— 那种配置会是在描述一个不存在的东西。
  → `joy-rs/joyczl-config/src/lib.rs`（`approval`）
* **刻意** —— 「记住这条命令」写进 `settings.json` 的是**被批准的那条命令本身**
  （不加通配），而且要**下次启动**才生效：执行策略在启动时读一次。说了「记住了」
  却悄悄把规则放宽，比不记更糟。
  → `joy-rs/joyczl-app-server/src/lib.rs`（`remember_command`）
* **刻意** —— MCP 的反向请求（elicitation/sampling）会收到一个明确的「不支持」，
  而不是完整支持。要真的回答它们，得先有一套「服务器随便问什么都答得出来」的
  UI 契约。
  → `joy-rs/joyczl-mcp/src/transport.rs`
* **刻意** —— 沙箱不可用就什么都不跑。Linux 上这意味着 `bwrap` 既要存在、
  也要真的能建命名空间：Ubuntu 24.04 限制了非特权用户命名空间，所以可用性
  是**每个进程探测一次**（真跑一次 `bwrap`），不是查文件在不在。
  → `joy-rs/joyczl-tools/src/exec.rs`（`sandbox_backend`、`bubblewrap_runs`）
* **未做** —— `JOY_EXEC` / `JOY_EXEC_ALLOW` / `JOY_WRITE` 只在启动时读一次，`config/write`
  不会重建工具表。改了要重启进程。
  → `joy-rs/joyczl-app-server/src/lib.rs`（`builtin_tools`）
* **未做** —— 硬拒名单是子串匹配加一个手写的「管道进 shell」识别。花哨的绕法
  （`bash -c "$(printf …)"`）它抓不住。真正的防线是放行表与沙箱。
  → `joy-rs/joyczl-tools/src/exec.rs`（`HARD_DENY`、`pipes_into_a_shell`）

* **刻意** —— 模型调用只对 429/5xx/网络抖动重试，次数受 `JOY_LLM_RETRIES`
  约束（默认 2），总预算 30 秒，而且**绝不换厂商**。某家挂了就是挂了，直到你
  改点什么。
  → `joy-rs/joyczl-provider/src/retry.rs`

* **刻意** —— 子代理报告的「转述加固」只是**减速带，不是沙箱**：它给模仿角色前缀
  （`Human:` / `系统:`）与我们的控制标记（`<system-reminder>`、`[tools used:`）的行
  加反斜杠，并加一句「这是转述」的声明。它**不判断恶意**（那做不到，还会带来虚假
  安全感），也**不改变任何工具调用的权限检查** —— 权限是闸门的事。真正的防线是
  沙箱、放行表与批准。
  → `joy-rs/joyczl-tools/src/report.rs`
* **刻意** —— 子代理没有批准通道：父轮会问你的地方，子代理直接按拒绝处理
  （`JOY_APPROVAL` 到不了它那儿）。一个阻塞在人类身上的子代理，比一个做不成
  这件事的子代理更糟。
* **刻意** —— 子代理的对话不留档（`session/list` 里不会冒出 `subagent:*`），
  它内部的事件也不外发；父轮只看到一次工具调用与结论。它用过的工具（含失败的
  调用）附在结论后面。
  → `joy-rs/joyczl-app-server/src/subagent.rs`

* **刻意** —— 记忆去重的键是 `(subject, 规范化 content)`（大小写与首尾空白不算
  区别），**不含 kind**：同一句话先以 `fact` 记下、后来被标成 `feedback` 时，留下
  的是**先到的那条**的分类。判重不跨语言（同一句话的中英两版是两条）。
  → `joy-rs/joyczl-state/migrations/0008_memory_dedup.sql`
* **刻意** —— 记忆类别是固定的五种（`fact` 兜底）；模型自创的分类会落进
  `fact`，而不是长出一套分类学。检索目前**不按类别过滤** —— 它被存下来、被
  返回，但还没被用来筛。
  → `joy-rs/joyczl-state/src/facts.rs`（`KINDS`）

* **刻意** —— 沙箱是 Unix-only（macOS `sandbox-exec`、Linux `bubblewrap`），
  而且这条边界在门口就执行：Windows 原生环境下 `joy` 二进制**拒绝启动**（exit 2）
  并打印原因——「看起来健康但处处受限的半成品」不如一声清楚的不。Windows 上
  跑 Joy 请用 WSL2（矩阵见 docs/operations.zh.md）。CI 有一个 Windows job 跑除
  沙箱测试之外的全部，名字里就写着这条边界。
  → `.github/workflows/ci.yml`、`joy-rs/joyczl-cli/src/main.rs`（`windows_blocker`）、
  `joy-rs/joyczl-tools/src/exec.rs`
* **刻意** —— 上下文压缩**不保留文件操作状态**（这个会话里读过/写过哪些文件，
  pi 的 harness 就是那么做的）。被挤出窗口之后留下的是滚动摘要与
  `[tools used: …]` 的折叠；模型要精确的早先改动，就自己把文件再读一遍。
  → `joy-rs/joyczl-memory/src/compaction.rs`、`joy-rs/joyczl-app-server/src/turn.rs`
* **刻意** —— token 数是拿 tiktoken 的编码**估**出来的，还用在并不使用它的厂商
  身上。这个估算只决定**什么时候**压缩，从不参与计费：权威数字是 provider 回报的
  `usage`。估错了只会让压缩早一点或晚一点发生，不会算错账。
  → `joy-rs/joyczl-provider/src/tokens.rs`

* **刻意** —— MCP 熔断是**进程内**的：参数写死为「连续 3 次失败 → 60 秒」，
  不做持久化也不可配（一台服务器重启后 Joy 仍会先试一次）。它防的是「每一轮都
  白等一个超时」，不是「精确的可用性统计」。
  → `joy-rs/joyczl-mcp/src/lib.rs`（`Breaker`）
* **刻意** —— 轮内工具结果预算只处理「单条很大的」结果。一百条各自都不大的结果
  把预算撑爆时，它**不改内容**，只在 stderr 记一行：给每条都建一个文件比省下的
  上下文更贵。
  → `joy-rs/joyczl-loop/src/budget.rs`
* **刻意** —— 实测校准用的是**比值**（实测 / 估算），夹在 0.5–2.0：一次异常请求
  不该把预算带偏。要更准只能为每家厂商各带一份真实词表。
  → `joy-rs/joyczl-state/src/chat.rs`（`context_factor`）

* **刻意** —— 钩子有 **两种 handler**：shell（`command`）与 HTTP（`url`——同一
  payload POST 过去、同一决策 JSON 回来，reqwest 本来就在依赖树里）。动词固定
  POST：payload 本身就是消息；加一个没人会填的 `method` 字段是发明。仍不做：
  Claude Code 的 mcp_tool / prompt / agent 三类——各自要一套契约，而 shell 已经
  能表达它们（`joy mcp`、模型 CLI）。要 LLM 判定就自己调模型。
* **刻意** —— 钩子是 **Unix 优先**的：命令用 `sh` 跑，Windows 上得自己准备可执行
  文件（CI 的 Windows job 跳过这些测试，而不是假装它们过）。真要在 Windows 上当
  一等公民，就补一条 `cmd /C` 分支（以及一套 Windows 上的钩子测试）。
* **刻意** —— 运行中改过的 `hooks.json` **不执行**（说清原因），要重启才按新内容来。
  这是拿一点方便换「没人能悄悄换掉你的闸门」。
* **刻意** —— 阻断 `Stop` 只让这一轮**再跑一次**（上限 1）。真正的「自动续轮」是
  目标循环的事，带轮次上限、判断器与人类授权边界。

* **刻意** —— 待办清单是**进程内**的：一个会话一份，重启就重来。它是这一趟的
  工作记忆，要留下来的东西走 `save_note` 与 `schedule`。它也有硬上限（20 项、
  单项 4000 字符、同时只能一项 `in_progress`）—— 清单是给人看的工作记忆，不是日志。
  子代理的表里没有它：那张表属于**父轮**那条任务线。
  → `joy-rs/joyczl-tools/src/todo.rs`

* **部分刻意** —— 后台作业的输出环仍是进程内 64 KiB（溢出丢最旧、如实说明），
  但「随进程生死」已经修掉一半：起跑/收尾写进 state.db 的 jobs 台账（迁移
  0009），输出随写随落到 `<home>/jobs/<pid>-<id>.log`（1 MiB 封顶，超了明说）。
  进程崩溃后，启动对账把还挂着的作业标成 `interrupted`、输出保留可回查 ——
  **不自动重跑**（参考的几家 harness 一致的选择：自动重跑是事故放大器，重跑
  的决定留给看着台账的人或模型）。跑完的结果仍落 `<home>/outbox/jobs/`。
  要「重启后接着跑」，写 `joy schedule` 的定时任务 —— 那条路会补跑错过的触发。
* **刻意** —— 后台命令与前台**共用同一道闸门**（沙箱、断网、放行表、批准全一致）：
  后台不是绕过批准的侧门。它也不提供 PTY —— 需要交互的程序（`vi`、`top`）本来就
  不该丢到后台。

* **刻意** —— 目标循环的判断器**只看对话**：便宜模型、没有工具、没有检索。
  它要回答的是「这段对话说明目标达成了吗」，多看别的只会让它开始替模型干活。
  也正因为如此，它对「做完了吗」的判断可能不准 —— 所以它只能**续轮**，不能
  替人决定放行任何东西。
* **刻意** —— 目标只能由人设/改/清（`/goal`，写进任务行的 `goalCondition`）：模型没有这条路径。
  到达终止态（达成/做不到/超轮次/判断器挂了）之后循环不会自己再跑，等下一句人的话。

## 并发

* **已修（0.16）** —— 同一会话的两个 turn 不再并行：`turn/start` 先认领会话，
  第二个拿到 `SESSION_BUSY`。这道门是**进程内**的 —— 跨进程（REPL 与 hub 里的
  另一个 app-server）仍然互不知情，那要等共享空间的接管。`turn/interrupt` 照旧
  能进来，因为它不占用会话。写段锁与 SQLite 三守卫继续留着：它们防的是
  「跨进程双写」与「读写交错」。
  → `joy-rs/joyczl-app-server/src/lib.rs`（`begin_turn`）、`joyczl-state/src/chat.rs`
* **刻意** —— SQLite 的等锁上限是 **10 秒**（挂在连接选项上，每个连接都带上）。
  多进程同时写（REPL、dashboard、`joy mcp serve`、`joy schedule`）时排队而非
  直接失败；10 秒还不够就只能减小并发面，而不是把超时调得更长。
  → `joy-rs/joyczl-state/src/db.rs`
* **刻意** —— 有 per-session **写段锁**，但只在 turn 的写入段持有：compaction
  （`load_history` 内的 `refresh`）与尾部写段（对话日志 append / 校准 / 提炼 /
  MEMORY.md 镜像）。**模型调用段不持锁**——那会把会话卡死，interrupt 也进不来；
  `turn/interrupt` 本身不写会话状态，与这把锁正交。锁按会话惰性创建、常驻不删
  （删了会让两个并发 turn 拿到两把不同的锁）。SQLite 层的三守卫仍保留：它们防
  「双写损坏」，锁防的是「读写交错导致的乱序」，两者互补。
  → `joyczl-app-server/src/lib.rs`（`session_write_lock`）、`turn.rs`（两处加锁段）
* **未做** —— `prune_spill` 的启动清理与另一进程的写入有理论竞争（只删 7 天前
  的文件，实际概率极低）。

## 图工作流

* **刻意** —— 工作流拓扑是 **JSON DAG**（`joy-rs/joyczl-graph/src/workflows/
  definitions/*.json`），但它是 `include_str!` **编译期内嵌**的：没有
  `<home>/workflows/` 运行时加载或覆盖。理由：工作流是行为（路由白名单、
  max_visits 护栏都是它的安全属性），从磁盘加载行为意味着绕过 review 改变
  控制流。要改拓扑，改 JSON 并重新编译。装载期校验（无环、路由须为注册
  白名单内置、节点必须有绑定）见 `workflow_def.rs`。

## 模型路由

只有三个档位（`fast`/`chat`/`reasoning`）—— profile 上没有成本/速度标签
（用量台账本来就按模型记账）、没有 vision 档路由（图片跟着回合所在档走，按
厂商的 `vision` 标志闸门把关——见「文件与上传」）、没有同级降级链：被路由到
的 profile 解析失败时直接回落 active profile，不试同族其它档。L1 分类器只看
消息文本（前 4k 字符）——不看工具、不看历史、不看图片。

* **刻意** —— 模型目录（`<home>/models-catalog.json`，见 configuration）
  **只在启动时读一次**：切换模型是重新 resolve provider，不是重读目录；新加
  条目要重启。坏行各欠一行 stderr 并被跳过；坏文件退回内置表——绝不拦启动。
  → `joy-rs/joyczl-provider/src/catalog.rs`（`load`）
* **刻意** —— embedding 端点（混合检索）目前仍只从**内置**厂商表里选：只存在
  于目录里的自定义厂商没有 embeddings，混合检索退化到纯关键词（它本来就有
  的关闭/降级形态）。
  → `joy-rs/joyczl-provider/src/embed.rs`（`lookup`）

## 文件与上传

驾驶舱的上传走**两条路**：非图片文件是引用——文件落在 `<home>/uploads/<日期>/`
下，名字完全由服务端生成（用户输入只取扩展名、过白名单），消息里注入的是绝对
路径，模型想读就用 `read_file`。**图片是例外**：它作为真正的
`ContentBlock::Image` 块进消息——服务端读字节、base64 编码，并在回合开始前按
厂商的 `vision` 能力闸门把关（各厂商开关见 configuration）。附件的一切检查都
在回合开始前完成：`<home>` 包含性、存在性、按扩展名嗅探的媒体类型、单图
20 MiB 上限、**单边 4096 像素上限**（只读文件头量宽高，零依赖）。

* **未做** —— 附件**不随会话恢复重放**：`chat_log` 存的是消息文本（里面提到
  上传路径），历史重建走文本——恢复后的会话是「描述」那张图，而不是重新
  展示它。要补就得持久化字节（或恢复时做一次引用解析）。
  → `joy-rs/joyczl-app-server/src/turn.rs`（`resolve_attachments`、`load_history`）
* **刻意** —— 单边超过 4096 像素的图在本地直接拒，不在本地缩放：长截图
  （1120×11100 这种）字节数只有 2 MB 出头，远端却只回一句 `unsupported image`；
  而把它缩到 4096 高只会把字压糊，正确做法是分段发。自动切段要先解码再重编码，
  等于引入一个图像库，暂不做。
  → `joyczl-app-server/src/image_meta.rs`（`MAX_EDGE`、`size`）
* **刻意** —— `vision` 描述的是**这套配置的模型**，默认跟随厂商目录行，且宁可
  写 `false`：错标 `false` 只少一个能力；错标 `true` 要多跑一趟远端、换一个
  看不懂的 400。但这套模型自己声明了就听它的 —— 同一家既有看图模型也有纯文本
  模型。声明：`/model add <名字> <厂商> <模型> --vision`，等价于 `models.json`
  那条里写 `"vision": true`；要让整个厂商都算，再改 `models-catalog.json`。
  → `joyczl-config/src/models.rs`（`ModelProfile::vision`）、
  `joyczl-provider/src/lib.rs`（`Resolved::vision`）

## 记忆

* **刻意** —— 完全重复进不了库：迁移 0008 在 `(subject, lower(trim(content)))`
  上建了唯一索引，`add` 命中冲突时返回已有行并标记非新——提炼重跑或模型重复
  `save_note` 是噪音，不是错误。**没做**的是近似重复的合并：同一件事换个说法
  会落成第二行。补法是按相似度做一次合并遍历。
  → `joy-rs/joyczl-state/src/facts.rs`（`add`）、`joy-rs/joyczl-state/migrations/0008_memory_dedup.sql`
* **未做** —— `MEMORY.md` 是**生成的视图**，每轮之后从 `facts` + `episodes`
  重写。编辑它没有用，state.db 才是事实来源。要让它可编辑，需要一条写回路径
  与冲突规则。
  → `joy-rs/joyczl-memory/src/lib.rs`（`export_markdown`）
* **刻意** —— 临时陈述过滤是标记词表（中英并列），不是分类器。少见的表达方式
  会漏掉。
  → `joy-rs/joyczl-memory/src/consolidation.rs`（`TEMPORARY_MARKERS`）
* **未做** —— 技能触发只对 ASCII 字母数字分词，所以哪怕对话是中文，技能的触发
  词也得写成英文。补法是 CJK 分词，或给技能加一个 `triggers:` 字段。
  → `joy-rs/joyczl-memory/src/skills.rs`（`tokens`）

## 检索

* **刻意** —— 混合检索需要 embedding 端点（`JOY_EMBEDDINGS=1` 配
  `JOY_EMBED_MODEL`）。默认关：关键词检索不需要模型、不需要网络。
  → `joy-rs/joyczl-provider/src/embed.rs`
* **未做** —— 向量存在 `facts.embedding` 这个 JSON 列里，检索是全表扫描在
  Rust 侧比余弦。个人规模（几千条）完全够用，上百万条就不对了。这一列就是
  将来迁移的起点。
  → `joy-rs/joyczl-state/migrations/0005_embeddings.sql`
* **未做** —— 开关打开之前写下的事实没有向量，只能靠关键词那条腿，直到跑一次
  `joy memory reindex`。情景记忆完全不向量化（它自带日期，日期已经干了大半活）。
  → `joy-rs/joyczl-memory/src/retrieval.rs`
* **未做** —— `MIN_SIMILARITY = 0.30` 是手感常数，没有按模型校准。不同 embedding
  模型的相似度尺度不一样。
  → `joy-rs/joyczl-memory/src/retrieval.rs`

## 上下文

* **刻意** —— 滚动摘要住它自己的 `context_rollups` 表；`chat_log` 里的原始轮次
  一个字没动。所以「重看历史」看到的是真对话，不是摘要。别在没想清楚「归档是
  干什么用的」之前把这个「修」掉。
  → `joy-rs/joyczl-state/src/chat.rs`、`joy-rs/joyczl-memory/src/compaction.rs`
* **刻意** —— 摘要模型罢工时退化为确定性摘录（并在 prompt 里说明），而不是丢掉
  被挤出去的轮次。
  → `joy-rs/joyczl-memory/src/compaction.rs`（`fallback_summary`）
* **未做** —— 摘要不会被再摘要：一个很长的会话就是一段越来越长的文字。摘要的
  摘要（分层滚动）没实现。
  → `joy-rs/joyczl-memory/src/compaction.rs`（`roll_forward`）

## 轮次与协议

* **边界** —— 会话元数据（0.15）里只有**名字**与**工作目录**：没有归属、没有可见性、
  没有 `status`。0018 之前建的会话 `cwd` 为空，打开时目录选择器会**清空**（这是诚实的
  渲染：那条会话确实没选过目录），再选一次才会记住。
  → `joy-rs/joyczl-state/src/chat.rs`、`migrations/0018_sessions.sql`
* **刻意** —— `turn/list` 报的是**本进程内存里**在跑的轮次：另一个进程（REPL、hub 里的
  另一个 app-server）跑的那一轮不在里面。跨进程的「谁在跑」要等共享空间的接管机制。
  → `joy-rs/joyczl-app-server/src/lib.rs`（`active_turns`）
* **刻意** —— 运行中补充（0.16）的 `steer` 在**轮次边界**进入工作记忆，所以
  「这一轮最后一次模型调用已经发出」之后再补，要等下一次调用或下一轮才生效；回合收尾前
  一刻补的话仍会写进 `chat_log`（`meta.steer`），不会丢。要「这轮结束后再做」用
  `turn/follow-up`（migration 0019 `session_queue`，重启不丢、一次只放一条）。
  → `joy-rs/joyczl-loop/src/lib.rs`（`Steering`）、`joyczl-state/src/queue.rs`
* **刻意** —— follow-up 排队的句子要等**下一轮开始**才出现在 SSE 流里：一条流仍然只覆盖
  一轮，排队本身不发通知。
  → `joy-rs/joyczl-app-server/src/lib.rs`（`follow_up`）
* **刻意** —— `turn/steer` 也只找**本进程**在跑的回合。另一个进程里的那一轮，它看不见，
  也不会转发过去（会在你问的时候说「没有在跑的回合」）。
  → `joy-rs/joyczl-app-server/src/lib.rs`（`steer`）
* **刻意** —— `turn/interrupt` 取消的是 loop 的模型调用与正在跑的工具；它**不**
  取消 loop 之前的检索门与压缩调用。在那些阶段打断，要到 loop 开始时才生效。
  要让它们也可取消，得把令牌一路传进那几个调用。
  → `joy-rs/joyczl-app-server/src/turn.rs`
* **未做** —— `joy mcp serve` 只实现 `initialize`、`tools/list`、`tools/call`。
  MCP 的 `resources`、`prompts`、采样与通知都没有。
  → `joy-rs/joyczl-mcp/src/server.rs`
* **未做** —— MCP OAuth 要手工跑 `joy mcp login <名>`；refresh token 最终过期时
  没有后台重新授权。
  → `joy-rs/joyczl-mcp/src/oauth.rs`、`joy-rs/joyczl-cli/src/mcp_cmd.rs`
* **刻意** —— 多个进程可以同时开 `state.db`（app-server、REPL、`joy mcp serve`、
  `joy schedule`）—— WAL 加 `busy_timeout` 就是干这个的 —— 但没有任何东西协调
  跨进程的**轮次**。两个进程里的两轮对话会自由交错。
  → `joy-rs/joyczl-state/src/db.rs`
* **边界** —— 回合 checkpoint（0.10.0）在**工具结果边界**落盘：崩溃后续跑绝不
  重跑已完成的工具，但**死在工具执行半途的那一次**会在续跑时整只重跑——工具
  应当按幂等写（`run_command` 的输出落盘与审批台账已经在帮这件事）。
  → `joy-rs/joyczl-loop/src/lib.rs`（checkpoint 调用点）
* **边界** —— 续跑自动发生（启动对账），但只在**有进程启动时**：hub 池闲置驱逐
  后没人来敲门，续跑就等着下次访问——通知不补发，错过的人看对话日志。
  → `joy-rs/joyczl-app-server/src/resume.rs`
* **边界** —— 用户打断（`turn/interrupt`）与回合内报错把 checkpoint 标
  `abandoned`，**不**自动续跑；`open` 在进程结束之后只可能意味着崩溃。
  → `joy-rs/joyczl-app-server/src/turn.rs`（`finalize_turn`）
* **边界** —— 任务**不会自己说第一句话**：`task/create` 建的是目标 + 台账，推进仍由
  人的下一句话（或已有的回合）驱动。让任务自启动等于让模型给自己派活，不在这个
  方案里。
  → `joy-rs/joyczl-app-server/src/task.rs`（`create`）
* **刻意** —— 没有任务队列、没有并行调度器：同一会话可以有多个未结束的任务（0.14 去掉了
  「一会话一活跃任务」，任务成了独立实体，甚至可以不属于任何会话），但卡片只显示最新的那个，
  同一会话的回合本来就串行。
  → `joy-rs/joyczl-state/src/tasks.rs`（`create`）、`joy-rs/joyczl-app-server/src/task.rs`（`view_of`）
* **边界** —— 「完成」由**事实决定，不由模型的结论决定**：每轮结束检查 `fileExists` /
  `fileContains` / `toolsOk`，有任一条不过就**不调判断器**。只有这三类算验收条件——"报告写得
  好"这种没法量的不收；`FileContains` 是纯子串，永远不当正则用。
  → `joy-rs/joyczl-agent/src/verify.rs`
* **边界** —— 中途断掉的副作用**绝不重放**：停在 `running` 的步骤在启动时转成 `waiting`，
  等人确认。宁可停下来问一句，也不把同一次写入发两遍。
  → `joy-rs/joyczl-state/src/steps.rs`（`settle_running`）、`joy-rs/joyczl-app-server/src/lib.rs`
* **边界** —— 预算**每轮检查一次**，在下一次模型调用之前。单轮可能冲过上限（一次很长的模型
  调用），台账会如实报出超了多少，而不是假装在中途硬切。
  → `joy-rs/joyczl-app-server/src/turn.rs`（`budget_overrun`）
* **边界** —— 后台作业（jobs）**不**挂任务台账：作业是工具层的事实，任务是意图；
  任务的一轮里可以起作业，但两者不互相记账。要"任务的作业清单"得先发明这层关系。
  → `joy-rs/joyczl-state/src/jobs.rs`、`joy-rs/joyczl-app-server/src/task.rs`
* **边界** —— 重启时 `running` 的任务先标 `suspended`，回合若有 `open` checkpoint
  会自动续跑并把它翻回 `running`——**崩溃恢复是自动的，人类按的暂停不是**：暂停的
  checkpoint 标 `suspended`，只有 `task/resume` 会让它动。两者的区别只在状态字上。
  → `joy-rs/joyczl-app-server/src/resume.rs`、`joy-rs/joyczl-app-server/src/turn.rs`
## 连接器

* **边界** —— 连接器没有「重试 / 退避 / 限流」：一次发送失败就是把失败原样交回模型，
  由它决定下一步（改天再发、换人问、如实说没发成）。批量的第三方调用不在这个方案里。
  → `joy-rs/joyczl-connector/src/smtp.rs`、`joy-rs/joyczl-connector/src/feishu.rs`
* **边界** —— 飞书默认以**应用身份**（tenant_access_token）发送；以**某个人**的身份
  发需要一次浏览器授权（`joy connector feishu login`），token 落 0600 文件、用前续期。
  这条流程没有 PKCE，回调固定 `127.0.0.1:41766/callback` 且必须在控制台登记。
  续期是**轮换**的（旧 refresh token 用一次即失效），所以 app-server 用锁文件把续期
  串行化、在过期前十分钟自动续；但续期只发生在**长住的 app-server 里**：`joy connector`
  这类一次性命令走惰性路径，而且进程全关着的时候没人续（refresh token 自己过期了，
  只能重新 login）。微信仍是个人号没有可编程出站接口，不含在其中（入站是另一套东西，
  见 Chat gateways）。
  → `joy-rs/joyczl-connector/src/feishu_oauth.rs`
* **边界** —— 入站邮件是**轮询**（最小 30 秒），没有 IDLE 推送：新信最多晚一个间隔才
  出现。去重按 IMAP UID，而 UID 在邮箱重建后会从 0 重来 —— 那时旧索引会挡住同 UID 的
  新信（判断依据在 message_id，但表上的唯一键是 UID）。
  → `joy-rs/joyczl-connector/src/imap.rs`、`joy-rs/joyczl-state/src/inbox.rs`
* **边界** —— 入站正文只存纯文本：HTML 去标签、不还原排版；非 UTF-8 字符集按 UTF-8
  有损解码（不带编码表，见上一条的 GBK 说明）；附件不落盘。首次启动只记水位、不回溯
  历史。
  → `joy-rs/joyczl-connector/src/imap.rs`
* **刻意** —— 白名单是**精确匹配**（邮件另支持 `@域名`）：没有通配、没有正则、没有
  「先问一次以后都放」。名单只有人能改；改完下一回合才生效（工具注册发生在启动时）。
  → `joy-rs/joyczl-connector/src/config.rs`
* **边界** —— 凭证明文回退是既有 `SecretStore` 的语义（钥匙串不可用时写 0600 文件并
  如实报告），连接器不另造加密格式；`live` 开关与白名单仍只在文件里，UI 只读。
  → `joy-rs/joyczl-config/src/models.rs`（`SecretStore`）

## 网关

* **未做** —— Discord 断线重连是重新 IDENTIFY 而不是 RESUME，所以断的那几秒里
  说的话会丢。补上要记 `session_id` 与 `resume_gateway_url`。
  → `joy-ts/packages/gateway/src/discord.ts`
* **未做** —— 微信、飞书、Discord 三个网关都没有连过真平台，只对假服务端测过。
  → `joy-ts/packages/gateway/`
* **刻意** —— WhatsApp 与语音网关没有实现。语音在原 Python 项目里有，没有搬过来；
  本仓库没有在任何地方声称有。

## 运维

* **未做** —— 没有日志轮转。trace 按天一个文件（`traces/YYYY-MM-DD.jsonl`），
  但 `usage.jsonl` 会一直长。
  → `joy-rs/joyczl-app-server/src/trace.rs`
* **未做** —— `joy schedule` 与 `joy dashboard` 都是前台进程，守护（launchd /
  systemd / Docker restart policy）是运维的事。不做 daemon 化，也不写 PID 文件。
  → `joy-rs/joyczl-cli/src/schedule.rs`、本仓库的 `Dockerfile`
* **刻意** —— 定时任务按分钟粒度触发、一次一条；那一分钟进程没在跑就跳过。
  漏掉的执行不会补跑。
  → `joy-rs/joyczl-cli/src/schedule.rs`（`due_now`、`run`）
* **未做** —— Homebrew formula 对 `v0.5.0` 已经完整（真实 `sha256`，对着 tag 归档
  核过），但它住在仓库里、不在 tap 里：今天 `brew install ./packaging/homebrew/joy.rb`
  得先有 checkout，而发布 tap 是另一个决定。每次发版都要重算哈希。
  → `packaging/homebrew/joy.rb`

## 质量体系

* **刻意** —— 确定性 eval 跑在脚本化的 provider 上：不要 key、不要网络、每次都
  同一答案。judge 类 eval 需要真模型与真 key，所以是另一条命令（`joy judge`）。
  → `joy-rs/joyczl-eval/src/`、`evals/`
* **刻意** —— 需要这台机器上未必有的东西（python3、shell、可用的沙箱）的场景声明
  `prereq` 并跳过 —— 计为跳过，绝不计为通过。
  → `evals/deterministic/exec.jsonl`、`evals/deterministic/hooks.jsonl`
* **未做** —— embedding 那条路没有对着真 embedding 服务的端到端测试：融合与降级
  有单测，HTTP 调用本身没有。
  → `joy-rs/joyczl-memory/src/retrieval_tests.rs`

## 业务形态（任务 API / webhook / 档位）

* **刻意** —— `/api/run` 与同步模式的 webhook 都是等这一轮跑完才返回；要「立刻收下、
  稍后交付」就用 webhook 的 `callback`（202 + 投递）。两者都没有任务队列：异步投递
  三次不成（或进程中途退出）就丢了这一次投递 —— 答案仍在会话历史里，但不会重投。
  → `joy-rs/joyczl-ops/src/api.rs`（`run_headless`）、`webhooks.rs`（`deliver`）
* **刻意** —— 防重放只在 `signature: true` 时成立，而且靠**本地时钟**判新鲜度：机器
  时钟漂移超过 ±300 秒会把合法请求判成过期（或让旧请求多活一会儿）。plain
  `X-Joy-Secret` 模式根本不防重放 —— 那是它的定义，不是 bug。
  → `joy-rs/joyczl-ops/src/webhooks.rs`（`admit`、`SIGNATURE_WINDOW`）
* **刻意** —— 限流是**进程内**的每分钟计数：重启清零，多进程/多实例不共享。它挡的是
  「一个 hook 被反复打」，不是「这台机器上所有 hook 合计的负载」，也不区分调用方
  （没有 per-caller 桶）。
  → `joy-rs/joyczl-ops/src/webhooks.rs`（`Guard::rate`）
* **刻意** —— 入站 webhook 只在**单用户模式**存在。hub 模式下一个端口后面是很多
  home，而一次敲门没有「哪位用户」可挑，所以那里一律 404。
  → `joy-rs/joyczl-ops/src/api.rs`（`hook`）、`Gate::single_app`
* **刻意** —— 声明的档位（`tier`）优先于 `fixed` 模式：fixed 是默认策略，不是否决权。
  代价是 `model-routes.json` 里没配那个档位时会沿缺省链回落到 active —— 于是
  「声明了 reasoning 却跑在 chat 上」。`TurnMeta.routeReason` 会把这件事写出来
  （`tier:reasoning → chat-profile`），不会被藏起来。
  → `joy-rs/joyczl-app-server/src/route.rs`（`resolved_for_turn`）

## 配置

* **刻意** —— `config/write` 落到 `<home>/settings.json`，而这份补丁是**叠在**
  环境变量之上的。所以对那个字段来说，保存过的补丁比 `JOY_*` 说话更响，直到把它
  清掉（空串清掉 model 覆盖；全空的补丁会删掉文件）。
  → `joy-rs/joyczl-config/src/lib.rs`（`apply_patch`、`save_patch`）
