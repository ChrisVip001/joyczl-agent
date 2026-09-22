# 已知边界，以及要改从哪儿下手

[English](limitations.md) | 简体中文

这份清单里的每一条都是**已知**边界：要么是刻意的设计决定，要么是还没做的
工作。没有一条是「没想到的 bug」。每条都写明：边界是什么、为什么是这样、
想改的话先打开哪个文件。

读法约定：**刻意** = 这就是期望的行为，改动属于设计变更，先讨论；**未做** =
我们希望有，还没人做。

## 执行（`run_command`、`JOY_EXEC`）

* **刻意** —— 沙箱内的命令默认**离线**（`JOY_EXEC_NETWORK=1` 才联网）；写权限
  限制在工作目录、Joy 的 home 与临时目录，外加 `JOY_EXEC_WRITABLE_ROOTS` 里
  列出的目录。要下载东西的命令得同时打开这两个开关。
  → `joy-rs/joyczl-tools/src/exec.rs`（`sandbox_argv`）
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
* **未做** —— `JOY_EXEC` / `JOY_EXEC_ALLOW` 只在启动时读一次，`config/write`
  不会重建工具表。改了要重启进程。
  → `joy-rs/joyczl-app-server/src/lib.rs`（`builtin_tools`）
* **未做** —— 硬拒名单是子串匹配加一个手写的「管道进 shell」识别。花哨的绕法
  （`bash -c "$(printf …)"`）它抓不住。真正的防线是放行表与沙箱。
  → `joy-rs/joyczl-tools/src/exec.rs`（`HARD_DENY`、`pipes_into_a_shell`）

* **刻意** —— 模型调用只对 429/5xx/网络抖动重试，次数受 `JOY_LLM_RETRIES`
  约束（默认 2），总预算 30 秒，而且**绝不换厂商**。某家挂了就是挂了，直到你
  改点什么。
  → `joy-rs/joyczl-provider/src/retry.rs`

* **刻意** —— 子代理没有批准通道：父轮会问你的地方，子代理直接按拒绝处理
  （`JOY_APPROVAL` 到不了它那儿）。一个阻塞在人类身上的子代理，比一个做不成
  这件事的子代理更糟。
* **刻意** —— 子代理的对话不留档（`session/list` 里不会冒出 `subagent:*`），
  它内部的事件也不外发；父轮只看到一次工具调用与结论。它用过的工具（含失败的
  调用）附在结论后面。
  → `joy-rs/joyczl-app-server/src/subagent.rs`

* **刻意** —— 记忆类别是固定的五种（`fact` 兜底）；模型自创的分类会落进
  `fact`，而不是长出一套分类学。检索目前**不按类别过滤** —— 它被存下来、被
  返回，但还没被用来筛。
  → `joy-rs/joyczl-state/src/facts.rs`（`KINDS`）

* **刻意** —— 沙箱是 Unix-only（macOS `sandbox-exec`、Linux `bubblewrap`）。
  Windows 上没有后端，于是 `run_command` 什么都拒绝执行 —— 这是「没有沙箱就不
  执行」的本意，不是故障。CI 有一个 Windows job 跑除沙箱测试之外的全部，名字里
  就写着这条边界。
  → `.github/workflows/ci.yml`、`joy-rs/joyczl-tools/src/exec.rs`
* **刻意** —— 上下文压缩**不保留文件操作状态**（这个会话里读过/写过哪些文件，
  pi 的 harness 就是那么做的）。被挤出窗口之后留下的是滚动摘要与
  `[tools used: …]` 的折叠；模型要精确的早先改动，就自己把文件再读一遍。
  → `joy-rs/joyczl-memory/src/compaction.rs`、`joy-rs/joyczl-app-server/src/turn.rs`
* **刻意** —— token 数是拿 tiktoken 的编码**估**出来的，还用在并不使用它的厂商
  身上。这个估算只决定**什么时候**压缩，从不参与计费：权威数字是 provider 回报的
  `usage`。估错了只会让压缩早一点或晚一点发生，不会算错账。
  → `joy-rs/joyczl-provider/src/tokens.rs`

* **刻意** —— 轮内工具结果预算只处理「单条很大的」结果。一百条各自都不大的结果
  把预算撑爆时，它**不改内容**，只在 stderr 记一行：给每条都建一个文件比省下的
  上下文更贵。
  → `joy-rs/joyczl-loop/src/budget.rs`
* **刻意** —— 实测校准用的是**比值**（实测 / 估算），夹在 0.5–2.0：一次异常请求
  不该把预算带偏。要更准只能为每家厂商各带一份真实词表。
  → `joy-rs/joyczl-state/src/chat.rs`（`context_factor`）

## 记忆

* **未做** —— 写入时没有去重/合并。`facts` 没有唯一约束、`add` 总是插入，
  所以重复调用 `save_note` 或反复提炼会攒下近似重复的事实。要补的话是一个
  合并遍历（或 subject+content 上的唯一规则）。
  → `joy-rs/joyczl-state/src/facts.rs`、`joy-rs/joyczl-state/migrations/`
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
* **刻意** —— 需要这台机器上未必有的东西（python3、可用的沙箱）的场景声明
  `prereq` 并跳过 —— 计为跳过，绝不计为通过。
  → `evals/deterministic/exec.jsonl`
* **未做** —— embedding 那条路没有对着真 embedding 服务的端到端测试：融合与降级
  有单测，HTTP 调用本身没有。
  → `joy-rs/joyczl-memory/src/retrieval_tests.rs`

## 配置

* **刻意** —— `config/write` 落到 `<home>/settings.json`，而这份补丁是**叠在**
  环境变量之上的。所以对那个字段来说，保存过的补丁比 `JOY_*` 说话更响，直到把它
  清掉（空串清掉 model 覆盖；全空的补丁会删掉文件）。
  → `joy-rs/joyczl-config/src/lib.rs`（`apply_patch`、`save_patch`）
