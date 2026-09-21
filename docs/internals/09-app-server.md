# ⑨ 服务端：`joyczl-app-server`

这一层是**唯一持有 state.db 的进程**，也是把前面所有层拼在一起的地方。
一章读懂它，就理解了 Joy 的运行时。

目录：`joy-rs/joyczl-app-server/src/`
（`lib.rs`（Server 装配）、`turn.rs`（一轮的全流程）、`dispatch.rs`（13 个方法）、
`stdio.rs`（传输）、`trace.rs`（落盘））。

## 9.1 `Server`：字段与两把锁

```rust
#[derive(Clone)]
pub struct Server {
    pub(crate) facts: Facts, pub(crate) episodes: Episodes,
    pub(crate) chat: Chat,   pub(crate) calendar: Calendar,
    pub(crate) settings: Arc<RwLock<Settings>>,          // config/write 会改
    pub(crate) resolved: Arc<RwLock<Option<Resolved>>>,  // 同上；Option 允许无 key 启动
    pub(crate) tools: Arc<ToolRegistry>,
    pub(crate) turns: Arc<Mutex<HashMap<String, Arc<Interrupt>>>>,   // 在跑的 turn
    pub(crate) pool: SqlitePool,
}
```

`Clone` 是廉价且必要的：图里的 `full_agent` 节点是 `'static` 闭包，只能拿自己拥有
的东西，它拿的就是这份克隆。每个字段都是把手（句柄/池/Arc），clone 不复制状态。

两把锁的分工：

| 锁 | 护什么 | 为什么 |
|---|---|---|
| `RwLock<Settings>` / `RwLock<Option<Resolved>>` | `config/write` 的读改写 | 锁里**没有 await**，只护几微秒；读面 `settings()` / `resolved()` 各返回一份快照 |
| `Mutex<HashMap>`（`turns`） | 取消令牌表 | 登记/摘除/打断三个操作都是短临界区 |

`boot(pool, settings)` 的装配顺序有依赖：

```
1. resolve(&settings) → Ok 就 Some，Err 就 eprintln 并 None      ← 没 key 也要能启动
2. builtin_tools(&settings).await                                 ← 因为要连 MCP，是 async
3. 四个 state 句柄 + 三个 Arc
```

`open(settings)`（免费函数）是启动入口：

```
clone settings
  → load_patch(home) + apply_patch(…)      ← 环境是地基，settings.json 覆盖更晚更响（③）
  → ensure_home()
  → state::open(home/state.db)
  → Server::boot
```

`builtin_tools` 的三段：

```
1. handlers::build_default()                           ← 9 个内置工具（⑥）
2. if settings.exec_enabled { register(run_command) }   ← 默认关；开了才注册（⑥）
3. McpClient::connect(home/mcp.json) → warnings 打 stderr、每个工具注册进来
```

第 3 段的容错：连不上的服务器**只留一行警告**，Joy 照样启动；没有 `mcp.json`
就等于没配 MCP。

## 9.2 `run_turn`：17 步流水线

`turn.rs（run_turn）`。顺序本身是设计，逐步讲：

| # | 步骤 | 为什么在这个位置 |
|---|---|---|
| 1 | `settings = server.settings()`（快照）、`started = Instant::now()` | 一轮之内配置不变 |
| 2 | `resolved = server.resolved()`，为 `None` → `PROVIDER_ERROR`「模型还没配好：缺 API key…」 | 缺 key 的报错要说清「怎么办」（③ 的 `no_key_message` 是同一套语气） |
| 3 | `session_id`：空/纯空白 → `"default"` | |
| 4 | `turn_id = new_turn_id()`（`"t{纳秒}"`） | |
| 5 | **登记取消令牌** + `_guard = TurnGuard{…}` | 守卫先建，无论怎么退出都摘表 |
| 6 | 发 `TurnStarted` | 客户端据此显示用户消息 |
| 7 | **前门 `graph_route`**；返回 `None`（图关着/坏了）就调 `full_turn` | 图只能省时间，不能减能力 |
| 8 | 补发 `ToolCompleted` 通知（每个工具一条） | `ToolStarted` 在 loop 期间由 observer 发（⑨.4），这里补的是结束态 |
| 9 | `fold_tool_activity(reply, tool_calls)` | 见 9.3 |
| 10 | `quick = graph.route == Quick` | |
| 11 | 组装 `TurnMeta{gate, graph, iterations, latency_ms, tools, model（quick 时是小模型）, provider, usage, interrupted}` | 「你是什么模型」不会被自己答错 |
| 12 | `meta_json = to_string(&meta)` | |
| 13 | **trace 与 usage 各写一行** | 观测不是可选项，但写失败只喊 stderr |
| 14 | `chat.append_exchange(user, reply, session_id, "app-server", meta_json)` | assistant 行带 meta |
| 15 | `consolidate_if_due(...)` → `>0` 时发 `ConsolidationCompleted` | 攒够了才提炼（⑦） |
| 16 | `export_markdown(...)`（MEMORY.md 镜像） | 失败只 eprintln——「镜子」不该让整轮报错 |
| 17 | 发 `TurnCompleted`，**最后**发 JSON-RPC 应答 | 保证客户端先收全通知再收应答 |

第 17 步的顺序在 `stdio` 之外也成立：`dispatch` 对 `turn/start` 返回
`Outcome::SelfSent`，意思是「应答我已经自己发过了」，`handle` 就不再补发。

## 9.3 `fold_tool_activity`：防「重复预订会议」

```rust
calls.is_empty() → 原样返回 reply
否则 → "{reply}\n[tools used: {name}({output 前 200 字符}); …]"
```

**它防的 bug**：模型忘了自己上一轮已经做过，于是再做一次。历史里如果只有「好的，
我订好了」这句自然语言，下一轮它没有证据表明自己动过手。折一行工具活动进去，
证据就在工作记忆里。system prompt 里的 SOUL 也写了配套一句：
「`[tools used: …]` 是你过去几轮实际做过的事；已经做过的不要重复做」。

注意 `TurnCompleted.reply` 用的是**未折叠**的 `result.reply`（给用户看的是干净的
话），而落库的是折叠后的（给下一轮的模型看的是带证据的）。

## 9.4 `graph_route` 与 `full_turn`

`full_turn` 是完整路径，也是所有参数的汇合点：

```
1. 检索门：gate::should_retrieve(client, small_model, message) → GateDecision
   立刻发 GateDecided（含 decision / query / reason）
2. embedder：embeddings_enabled 时 Embedder::from_settings；失败只 eprintln（⑦）
3. retrieve_context（门说查才查）
4. 技能：SkillLoader::dirs_for(home) → matching_skills(message)
5. 工作记忆：load_history(chat, resolved, session_id, history_turns) → (history, summary)
6. build_system(soul, model, provider, memory, skills, summary)
7. on_text：把 delta 包成 TextDelta 通知
8. ToolCtx{ facts, episodes, chat, calendar, home }
9. observer 组装：tool_started（把 LoopEvent::ToolStart 翻成 ToolStarted 通知）
     inner 是图给的节点出口；两个都在时合成一个闭包（先发通知，再交给引擎打 node=）
10. joyczl_loop::run(Turn{…, observer, on_text, interrupt})
```

`build_system` 的拼接顺序（`"\n"` 连接）就是「模型每轮看到什么」：

```
1. SOUL.md 全文（不存在则写一份默认的再读）
2. "\nRight now it is {星期, YYYY-MM-DD HH:MM}（{时区名}, UTC±hh:mm）"
3. "Your model: you are running on '{model}' via the '{provider}' provider, inside Joy…"
4. "\nRelevant memory:\n{memory}"          ← 空则不加这一段
5. "\nRelevant skill instructions:\n{skills}"
6. summary_section(summary)                ← "\nEarlier in this conversation:\n…"
```

`graph_route` 的几点实现细节：

- 图关着 → `Ok(None)`（默认路径）。
- 建图失败、`run_graph` 出错 → `eprintln` + `Ok(None)`，掉回完整路径。
- **结果从侧信道回来**：`state` 只能装 JSON，而 `LoopResult` 不是 JSON，所以用
  `Arc<Mutex<Option<Result<FullTurn, ErrorObject>>>>` 传真结果。
- 分派：侧信道有 `Err` → 返回 `Err`；有 `Ok(turn)` → `GraphRouteKind::Full`；
  没有但 state 里有 `reply` → `GraphRouteKind::Quick`（构造一个「一次模型调用、
  没有工具」的 `LoopResult`），`gate` 为 `None`（快答路径连门都没过，如实标）。
- 事件：`Started/NodeStarted/NodeEnded/Ended` 翻成通知；`Route` 落进 `meta.graph`；
  `Inner` **不再翻**（loop 层已经发过了，翻两次会重复）。

## 9.5 `dispatch.rs`：13 个方法的实现

```rust
enum Outcome { Value(Value), SelfSent }
pub async fn handle(server, request, sink) -> ()
```

`handle` 保证**永远有一个应答送出去**：`Ok(Value)` → `Response`，`Ok(SelfSent)`
→ 什么都不做，`Err` → `Error`。

| 方法 | 关键实现 |
|---|---|
| `memory/search` | `top_k` 默认 4；facts 走 `search`，episodes 固定 3 条 |
| `memory/list` | `limit` 默认 50，`offset` 从 cursor 解析；`next_cursor = (rows.len()==limit).then(offset+limit)` |
| `memory/list-episodes` | `limit` 默认 20；`next_cursor: None`（`recent` 没有下一页） |
| `memory/remember` | `source` 空则 `"user"` |
| `memory/forget` | 返回 `removed`（`narrow` 到 i32） |
| `session/list` | 内存里切页 |
| `session/new` | **只是发一个新标签**（`s{毫秒}`），不建表不建行 |
| `session/messages` | `before` 从 cursor；`next_cursor` 按**原始行**算（`filter_map` 丢掉认不出的角色后仍用原始长度判断） |
| `config/read` | `settings().view()` |
| `config/write` | `apply_config_patch`，校验失败 → `INVALID_PARAMS`（③ 的六步） |
| `model/list` | 数据源就是 `PROVIDERS` 表；`flagship`/`fast` 两个展示位；无 cursor（11 行不需要翻页） |
| `turn/start` | 调 `run_turn`，返回 `SelfSent` |
| `turn/interrupt` | `interrupt_turn(&turn_id)` → `{interrupted: bool}`（找不到就诚实说 false） |

几个转换函数值得记住：

- `narrow(i64) -> i32`：存储是 i64、协议是 i32，越界**夹到边界**而不是 panic
  （记忆库真有 21 亿条的话，编号错乱也远好过整个进程崩）。
- `iso()`：SQLite 的 `datetime('now')` 是 `"%Y-%m-%d %H:%M:%S"`（UTC 但没有标记），
  补一个 `Z` 变成 RFC3339——不补的话浏览器会当本地时间，整体偏 8 小时。
- `to_message`：认不出的角色返回 `None`（**硬塞一个值就得撒谎**）。

## 9.6 `stdio.rs`：一个 writer、两个读法

```rust
let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();
```

- **通知与应答走同一个通道**，由**唯一**一个 writer 任务写 stdout。这样帧之间不会
  交错（两个任务同时写 stdout 会把两行 JSON 搅在一起）。
- writer 循环：`Frame::Notification` 包成 `{"method":"turn/notification","params":…}`，
  `Frame::Response` 直接序列化；写或 flush 失败就 break（客户端断了，剩余帧丢弃）。

读循环的行为：

| 情况 | 处理 | 为什么 |
|---|---|---|
| `turn/start` | `tokio::spawn(handle(...))` | 它一跑几十秒，读循环必须继续读，否则 `turn/interrupt` 永远进不来 |
| 其它方法 | 直接 `handle(...).await` | 按序处理：上一句的写入下一句要查得到（冒烟测试里的 remember→search→forget 依赖这个） |
| 解析失败 | 回 `PARSE_ERROR`，`id: null` | JSON-RPC 规定解析失败时 id 无从得知 |
| stdin 关闭 | `drop(tx)` 后等 writer 写完通道剩余帧 | 不丢最后几帧 |

**日志只能走 stderr**，因为 stdout 是协议通道：一行日志混进去就是一帧读不懂的
JSON。这条规矩在 `lib.rs`、`builtin_tools`、`exec`、`oauth` 等所有地方都遵守
（全部 `eprintln!`）。

## 9.7 `trace.rs`：观测的两个文件

| 函数 | 落点 |
|---|---|
| `record_turn(home, {ts, turnId, sessionId, userMessage, reply, meta})` | `home/traces/{Local 日期}.jsonl`（每轮一行） |
| `record_usage(home, {ts, turnId, sessionId, provider, model, inputTokens, outputTokens, iterations, interrupted})` | `home/usage.jsonl`（append-only 账本） |

`append_jsonl` 的失败策略：`create_dir_all` 失败或写失败都 `eprintln!` 后返回，
**绝不 panic**。理由：账本坏了不该连累对话，也不能悄悄闭嘴。
（已知边界：`usage.jsonl` 没有轮转，见 `docs/limitations.md`。）

## 9.8 测试钉住了什么

`turn_tests.rs`（19 条）+ `dispatch_tests.rs` 覆盖：

- 完整路径的事件顺序与 `TurnMeta` 内容（含 `usage` 落库、`interrupted` 为 false）；
- `ToolStarted` **必须先于** `ToolCompleted`（顺序错客户端就画不出「正在调用 X」）；
- `turn/interrupt` 打到在跑的 turn、打不到已结束的（且诚实回 false）；
- `config/write` 内存生效 + 落盘 + **重启后仍在**（同一个 home 再 `open` 一次）；
- 非法补丁被拒且**什么都没改**（内存还是默认值、盘上没有文件）；
- `model/list` 的目录与按 provider 过滤；
- 一轮跑完后 traces 与 usage.jsonl 各有一行、字段正确；
- MCP 连不上时不致命（用假 MCP 服务器跑）。

**这一层的不变量**：一轮的应答永远在所有通知之后；`TurnGuard` 保证取消令牌一定
被摘掉；`stdout` 只有协议帧；任何观测/镜像/提炼的失败都不影响这一轮的答复。
