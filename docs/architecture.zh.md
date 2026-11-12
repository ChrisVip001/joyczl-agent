# 技术架构

[English](architecture.md) | 简体中文

Joy 是一个本地优先的个人助手：状态、推理与工具执行全部发生在用户自己的
机器上。本文描述支撑这一目标的软件结构；运行与配置见
[operations.md](operations.zh.md) 与 [configuration.md](configuration.zh.md)。

## 设计原则

1. **Rust 独占逻辑与状态。** agent 循环、记忆、工具、SQLite 状态库全部在
   Rust；crate 按职责切碎（目标 <500 行/模块），不设 `joy-core` 大包。
2. **协议单一定义。** `joyczl-protocol` 是跨语言契约的唯一事实来源：
   `#[derive(TS)]` 生成 TypeScript、`#[derive(JsonSchema)]` 生成 JSON
   Schema 再生成 Python pydantic 模型，生成物 check-in 进仓库。
3. **客户端只做翻译。** CLI、dashboard、聊天网关、Python SDK 都是
   `joy app-server`（JSON-RPC over stdio）的客户端。state.db 只在一个
   进程里打开。

## crate 依赖图

```
                 joyczl-protocol  ←──── 唯一事实来源（TS/JSON Schema/pydantic 生成物）
                 joyczl-config    ←──── JOY_* 环境变量，启动读一次
                        │
                 joyczl-provider ────── 12 家厂商（含本地 Ollama），两种 wire format + SSE
                        │
        ┌───────────────┼────────────────┐
   joyczl-tools    joyczl-loop       joyczl-mcp
   （工具注册表）   （THE LOOP）      （MCP 客户端 + OAuth）
        │               │
        └───────┬───────┘
           joyczl-memory          joyczl-graph
        （门/提炼/Skills）      （波次 DAG 引擎 + triage/gather）
                │
         joyczl-state ────────── SQLite + FTS5(trigram)，sqlx 迁移
                │
        joyczl-agent ────────── 确定性内核：验收检查、工具事实观察、执行计划、
                │               以及全仓唯一那条路径包含规则
                │
        joyczl-app-server ──── JSON-RPC over stdio，唯一持有 state.db
          │         │
     joyczl-cli  joyczl-ops（dashboard：axum + SSE）
          │
     joyczl-eval（确定性 eval / judge / release gate）
```

依赖方向自上而下、无环。`joyczl-eval` 与所有客户端一样经由公共 API
（`Server`、`run_turn`、`install_provider`）驱动核心，是架构正确性的
日常证明：eval 能黑盒驱动的东西，任何前端都能。

## 一轮 turn 的数据流

```
用户消息
  → [triage 图，可选] classify（小模型）∥ check_calendar → quick | full
  → [检索门] 小模型判断要不要翻记忆（失败开放：门坏了就照常检索）
  → [记忆检索] facts (FTS5, top_k) + episodes → 进提示的尾巴
  → [Skills] 每轮带目录（名字 + 一句话），命中消息才载入正文
  → [提示装配] 稳定前缀（灵魂 + 模型身份）＋ 每轮尾巴
      （时钟 · 能力缺口 · 待办 · 记忆命中 · 两份目录 · 滚动摘要）
      → 同一会话里前缀逐字节不变：这就是缓存断点
  → [THE LOOP] observe → reason → act → repeat
        · 每次模型调用与「取消令牌」竞速（turn/interrupt）
        · 每次工具执行同样竞速；错误作为文本回给模型，不崩
        · 最后一步是收尾轮：工具收起，让模型自己说「完成 / 卡住 / 需要什么」
  → [账本] 逐行追加（user / assistant / tool_use / tool_result）；
      超过 JOY_LEDGER_INLINE_MAX 的内容转存到 <home>/ledger/
  → [落库] 对话 + meta（gate/graph/tools/model/usage）入 chat_log
  → [consolidation] 攒够 N 轮，小模型蒸馏出 facts + episode
  → [bundle] 后台 worker 把它们写成 <home>/memory/ 的页，并重建索引
  → [镜像] MEMORY.md（人类可读视图）、traces/<日期>.jsonl、usage.jsonl
  → [通知] turnStarted → gateDecided → textDelta → toolStarted →
           toolCompleted → consolidationCompleted → turnCompleted
```

## 关键机制

### 状态层（joyczl-state）

单个 `state.db`：`facts` / `episodes`（各带 FTS5 trigram 索引与同步触发
器）、`chat_log` + `sessions`（0018：会话有了自己的一行——名字与工作目录，
正文仍在 `chat_log`）、`calendar_events`（(title, start)
唯一索引提供 SQL 层幂等）。迁移由 `sqlx::migrate!` 编译期内嵌，二进制
自带 schema。中文检索靠 trigram + LIKE 兜底。记忆后端契约见
`store.rs`：`SemanticStore` / `EpisodicStore` 两个 trait +
`conformance` 验收，加第二个后端 = 实现接口 + 跑同一套测试。

### 记忆决策层（joyczl-memory）

* **检索门**：小模型判断"这条消息要不要翻记忆"，输出 JSON；任何失败
  都开放为检索——过时的记忆好过丢失的记忆。
* **混合检索**：关键词命中（FTS5）与向量邻居按名次融合（RRF），所以
  「上次聊的那个上线安排」能找到「10 月 15 日发版」。默认关着；embedding
  服务不可用时降级为纯关键词。
* **consolidation**：每 N 轮把未提炼对话蒸馏成 facts（标注来源）与一条
  episode；失败不丢原始日志。
* **压缩**：被工作记忆滑窗挤出去的轮次折进按会话的滚动摘要（存 state.db、
  只往前滚、不重算全史）。摘要模型罢工时退化为确定性摘录 —— 挤出窗口
  不等于失忆。
* **Skills（过程记忆）**：`SKILL.md`（Agent Skills 格式）渐进披露——
  frontmatter 常扫，正文只在消息与描述关键词重合 ≥2 时进 prompt。
  `MEMORY.md` 是每轮重建的人类可读镜像，state.db 永远是事实来源。
* **bundle（派生知识）**：后台 worker 把 facts 蒸馏成 `<home>/memory/` 里的一页页
  （OKF 形态，一个概念一个文件），并在上面维护 FTS5 索引。提示里带的是**目录**，不是页；
  模型用 `read_memory_page` 按需取。人也可以手改某一页 —— 下一次 `joy memory reindex`
  以文件为准。
* **账本优先**：每一轮都逐行追加进 `state.db` 的 `ledger`（每行一个 digest，超限内容转存
  `<home>/ledger/`），记忆中心就是从它把一轮读回来的。上面那些层都不是任何东西的唯一副本。

### 上下文预算（模型每轮到底看到什么）

模型每轮看到的东西都在 `build_system` 里装配一次，然后由记忆中心计量成六段：

| 段 | 来源 | 旋钮 |
|---|---|---|
| soul | `SOUL.md`（薄 / 全） | `JOY_SOUL` |
| memory | 检索门命中 + bundle 目录 + 技能目录 | `JOY_RETRIEVAL_TOP_K` |
| skills | 这条消息命中的技能正文 | — |
| tools | 已注册工具的 JSON schema | `JOY_TOOL_POLICY=deferred` 会把它压小 |
| history | 账本里的最近 N 轮 | `JOY_HISTORY_TURNS` |
| summary | 滚动压缩摘要 | `JOY_COMPACT_THRESHOLD` |

三件事拦住它悄悄变大：**压缩**把被挤出去的轮次折进滚动摘要（摘要模型罢工时退化为确定性
摘录，挤出窗口不等于失忆）；**工具结果换桩**在每次请求前把最旧的超大结果换成桩
（`JOY_TOOL_RESULT_TOTAL_CHARS`，最新的 `JOY_TOOL_RESULT_KEEP_RECENT` 条整条留着）；
**迭代预算**限制这一轮本身（`JOY_MAX_ITERATIONS`，最后一步是收尾轮）。

尾巴里还有几行很短的东西 —— 时钟、待办板、能力缺口说明 —— 每轮几十个 token，**故意不计入**
那六段：六段那条带子是用来看「什么在长」的，混进常量只会变成噪音。

### 工具层（joyczl-tools）

二十多个内置工具 + MCP 外挂，统一进 `ToolRegistry`。契约：`execute`
永不返回 Err——错误作为 `Error: …` 文本回给模型，loop 不崩。工具失败
是模型可以纠正的信息，不是崩溃理由。

### 图引擎（joyczl-graph）

波次执行的确定性 DAG：state 是黑板（并行节点写同一键 = 引擎级报错）、
路由器是纯代码函数（控制流绝不交给模型）、每节点 `max_visits` + 全局
`max_steps` 双护栏。两个工作流：**triage**（闲聊走小模型快答，其余进
完整 loop；任何失败开放回普通路径）与 **gather**（晨报：四路并行扫描
→ 一次无工具综合 → 按计数路由出草稿；只提议，绝不行动）。

### 打断（turn/interrupt）

每轮注册一个取消令牌；模型调用与工具执行都通过 `tokio::select!` 与
取消竞速，输掉的 future 被丢弃（HTTP 连接随之关闭）。半截回复如实
落库，`meta.interrupted = true`。

### 评测（joyczl-eval）

确定性 eval 进程内驱动同一个 `run_turn` + scripted 模型：离线、0/1、
可重复，场景为 `evals/deterministic/*.jsonl`。**100% 通过 = release
gate**（`joy eval` 的退出码）。judge 用真模型答题、便宜模型按 rubric
打 0-10 分，出分不拦发版。报告落 `eval_report.json`，历史追加
`eval_runs.jsonl`。

## 安全边界

API key 只存在于环境变量，不进任何文件与协议载荷；MCP OAuth token
落 `mcp-auth/`（0600，先写临时文件再改名）；`send_message` 只出草稿；
gather 图结构性无工具；技能名强制 slug 防路径穿越。完整清单见
[SECURITY.md](../SECURITY.md)。

## 设计决策

### 为什么中文检索需要专门的迁移

FTS5 默认的 `unicode61` 分词器把一整串连续中文当成一个词，于是「早上」
匹配不到「阿明喜欢早上的会议」。`migrations/0002` 换成 trigram（按连续
三字符切片），中文天然可用；trigram 切不出三元组的最短查询词（中文两字
词很常见）由 LIKE 子串扫描兜底——个人记忆库几千行，一次全表 LIKE 是
微秒级。

### 为什么生成物要 check-in

「改了 Rust 协议却忘了重新生成」是一类不会报错、只会让前端静默读错的
bug。把生成物提交进仓库，这类漂移就会出现在 PR diff 里；CI 用
`--check` 兜底。

### 为什么生成逻辑只在 test 构建下编译

`joyczl-protocol` 的线上依赖只有 `serde` 和 `serde_json`——`ts-rs` 和
`schemars` 是 dev-dependency，由 `joyczl-protocol-noop-macros` 切换：

```rust
#[cfg(test)]      pub(crate) use ts_rs::TS;
#[cfg(not(test))] pub(crate) use joyczl_protocol_noop_macros::TS;   // 空宏
```

所以 `cargo build` 完全不碰代码生成器，二进制里也没有它们。

## 记忆的三层

* **账本是事实。** `state.db` 里的 `ledger` 表，只追加：一轮里的 user / assistant /
  tool_use / tool_result / image 各成一行，每行对规范化后的 payload 求 `digest`。超过
  `JOY_LEDGER_INLINE_MAX` 的内容转存到 `<home>/ledger/…`，行里留指针、摘要与字节数。
  任意一轮都能从账本重建 —— 历史装配读的是账本，不是一段被折叠过的文本。走一次性回填
  进来的行带 `fidelity: folded`，这是「这段工具输出只以摘要形式存在」的诚实标签。
* **bundle 是派生的知识。** `<home>/memory/` 按 OKF 的形态：一个概念一页 Markdown，
  frontmatter 里写 `title` / `kind` / `sources`，`index.md` 是目录，`changelog.md` 是变更史。
  人可以随手改任一页；改过之后那一页的 digest 与索引对不上，下次 `joy memory reindex`
  以**文件**为准重建索引。
* **索引是缓存。** 检索门后面的 SQLite FTS5。丢了它只损失一次重建，绝不损失事实。
  `MEMORY.md` 是目录的生成副本，只为过渡保留。

提示里带的是前两层的**目录**，不是百科。新会话从一张目录开始，需要时按路径取页
（`read_memory_page`）—— 这和技能目录、延迟加载的工具表是同一个动作：把目录交给模型，
让它自己决定拿哪一本，而不是每轮把整架书塞过去。
