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
  → [记忆检索] facts (FTS5, top_k) + episodes → 拼进 system prompt
  → [Skills] 扫描 SKILL.md frontmatter，命中消息才载入正文
  → [THE LOOP] observe → reason → act → repeat
        · 每次模型调用与「取消令牌」竞速（turn/interrupt）
        · 每次工具执行同样竞速；错误作为文本回给模型，不崩
  → [落库] 对话 + meta（gate/graph/tools/model/usage）入 chat_log
  → [consolidation] 攒够 N 轮，小模型蒸馏出 facts + episode
  → [镜像] MEMORY.md（人类可读视图）、traces/<日期>.jsonl、usage.jsonl
  → [通知] turnStarted → gateDecided → textDelta → toolStarted →
           toolCompleted → consolidationCompleted → turnCompleted
```

## 关键机制

### 状态层（joyczl-state）

单个 `state.db`：`facts` / `episodes`（各带 FTS5 trigram 索引与同步触发
器）、`chat_log`（会话只是标签列）、`calendar_events`（(title, start)
唯一索引提供 SQL 层幂等）。迁移由 `sqlx::migrate!` 编译期内嵌，二进制
自带 schema。中文检索靠 trigram + LIKE 兜底。记忆后端契约见
`store.rs`：`SemanticStore` / `EpisodicStore` 两个 trait +
`conformance` 验收，加第二个后端 = 实现接口 + 跑同一套测试。

### 记忆决策层（joyczl-memory）

* **检索门**：小模型判断"这条消息要不要翻记忆"，输出 JSON；任何失败
  都开放为检索——过时的记忆好过丢失的记忆。
* **consolidation**：每 N 轮把未提炼对话蒸馏成 facts（标注来源）与一条
  episode；失败不丢原始日志。
* **Skills（过程记忆）**：`SKILL.md`（Agent Skills 格式）渐进披露——
  frontmatter 常扫，正文只在消息与描述关键词重合 ≥2 时进 prompt。
  `MEMORY.md` 是每轮重建的人类可读镜像，state.db 永远是事实来源。

### 工具层（joyczl-tools）

11 个内置工具 + MCP 外挂，统一进 `ToolRegistry`。契约：`execute`
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
