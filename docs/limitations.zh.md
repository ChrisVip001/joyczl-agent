# 已知边界，以及要改从哪儿下手

[English](limitations.md) | 简体中文

这份清单里的每一条都是**已知**边界：要么是刻意的设计决定，要么是还没做的
工作。没有一条是「没想到的 bug」。每条都写明：边界是什么、为什么是这样、
想改的话先打开哪个文件。

读法约定：**刻意** = 这就是期望的行为，改动属于设计变更，先讨论；**未做** =
我们希望有，还没人做。

## 执行（`run_command`、`JOY_EXEC`）

* **刻意** —— 沙箱限制的是**写路径**，不是网络。沙箱内的命令照样能联网。
  要断开需要各平台各写一层：macOS 的 seatbelt `network*` 规则、Linux 的
  `--unshare-net`。
  → `joy-rs/joyczl-tools/src/exec.rs`（`sandbox_command`）
* **刻意** —— 没有交互式批准弹窗。第三道闸门是放行表，不是对话框。做弹窗要
  动协议方法加驾驶舱 UI，那是独立的一个项目。
  → `joy-rs/joyczl-tools/src/exec.rs`（`vet`）、`joy-rs/joyczl-app-server/src/dispatch.rs`
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
