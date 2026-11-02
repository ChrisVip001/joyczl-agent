# ⑧ 图引擎：`joyczl-graph`

这一层解决一件事：**把「几条并行的事 + 一次汇合 + 一个判断」写成数据，而不是写成
一堆 if。** 它很像 LangGraph，但整个引擎加拓扑描述不到 800 行，且节点是 Rust 闭包而不是任意 Python。

目录：`joy-rs/joyczl-graph/src/`
（`lib.rs`（引擎）、`nodes.rs`（常用节点形状）、`workflows/`（triage、gather））。

## 8.1 概念与类型

```rust
START / END                     // 两个虚拟节点名
DEFAULT_MAX_STEPS = 25

pub struct State(BTreeMap<String, Value>);     // 黑板：键值对，键有序（并行合并顺序确定）

pub struct NodeCtx { state: State, inner: LoopObserver }
   // state 是**快照**（改它不影响别人，想留下来要返回）；inner 是节点内部事件出口

pub type NodeWrites = Map<String, Value>;
pub type Boxed<T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send>>;

pub struct Node { name, kind, max_visits (默认 1), on_error: Option<String>, f: NodeFn }
pub struct Graph { name, nodes: Vec<Node>, index, edges: Vec<Edge>, routers: BTreeMap<String, Router> }
pub struct Topology { name, nodes, edges }     // describe() 的产物，给驾驶舱画图用
```

三个设计细节：

- **state 是快照**：并行节点各拿一份**同一个**快照，各自返回自己要写的键；引擎
  负责合并。这消除了「谁先写」的不确定性。
- **`Vec<Node>` 的插入顺序是语义的一部分**：同波次节点的执行顺序与合并顺序都按它。
- `writes([("key", value), …])` 是拼返回值的助手；契约是键为 `&'static str`
  （键名不该在运行时拼出来——写错一个键名应该是编译期的事）。

## 8.2 `run_graph` 的执行算法（这一层的全部内容）

```
输入：graph、初始 state、observer、max_steps

1. 发 Started{ workflow, nodes }
2. START 先点边：src == START 的边把 dst 加入 fired
3. wave = next_wave(…)                      ← 见下

while wave 非空:
  若 path.len() + wave.len() > max_steps → errors["engine"] = "max_steps=… reached"，break
  对每个 wave 成员：runs[name] += 1 得 visit，发 NodeStarted{ node, visit }
  results = run_wave(…)                     ← 同波**并发**跑
  按波次顺序合并每个结果：
      path.push(name)
      对返回的每个「非 `_` 开头」的键：
         若同波里**别的节点**已经写过这个键 → return Err(GraphError::Collision{…})
         否则写进 state
      发 NodeEnded{ node, ms, keys, error }
      若 error：errors.insert(name, msg)；有 on_error 则把它加入 jumps；continue
      否则看 router：
         Some(router) → label = (router.route)(&state)
                        target = router.targets.get(&label)
                        发 Route{ router, target 或 END, reason: label }
                        未识别的 label → errors.insert(name, "router returned unknown label …")
                        target != END → jumps.push(target)      ← 路由是**跳**，不是依赖
         None         → 遍历 edges，src == name && dst != END 的点着 dst
  wave = next_wave(…, jumps, …)

收尾：发 Ended{ workflow, ms, steps, path, error }
      把 errors 以对象写回 state["errors"]
      返回 RunReport{ state, path, errors, ms }
```

`next_wave`（波次计算）：

```
ready = 插入顺序过滤：deps 非空 && 该节点的全部静态入边都已在 fired 里 && 还没跑过
遍历顺序：**先 jumps，再 ready**
跳过：END、wave 内重复、不存在的名字
若 runs[name] >= node.max_visits → errors 记 "max_visits=… reached" 并跳过
```

三个由此得到的性质：

1. **并行发生在同一波内**（`join_all`），且只看同一份快照——所以同波节点必须写
   **不相交的键**，否则是图写错了（`Collision`，宁可报错也不静默覆盖）。
2. **路由是跳不是依赖**：`jumps` 优先于静态入边，所以「条件分支」不会因为静态边
   而把两条分支都跑起来。
3. **出错不传播成 Err**：节点抛错只让「它的出边不触发」，运行自然排空到 END 并
   把错误记进报告。图上任一节点挂了，整张图仍然是「跑完的」，错误在报告里。

## 8.3 事件：`GraphEvent`

| 事件 | 时机 |
|---|---|
| `Started { workflow, nodes }` | 开跑 |
| `NodeStarted { node, visit }` | 每波开始（`visit>1` 说明在循环里） |
| `NodeEnded { node, ms, keys, error }` | 节点跑完（keys 不含 `_` 私有的） |
| `Route { router, target, reason }` | 路由器返回后 |
| `Inner { node, event: LoopEvent }` | 节点内部事件（loop 的 llm/text/tool），引擎补上 node= |
| `Ended { workflow, ms, steps, path, error }` | 收尾（path 是实际走过的节点序列） |

`Inner` 是「图里的 agent 节点」与「trace/通知」之间的桥：节点内部跑了一整轮 loop，
它的 `Text`/`Tool` 事件从这儿出来，引擎打上节点标签再转给上层——于是驾驶舱能把
stream 按节点分组显示。

## 8.4 `nodes.rs`：两种常用形状

```rust
key_router("route", "full")   // RouteFn：state 里 route 非空就用它，否则用默认
fn_node(|state| writes([…]))  // 把「state 进、键出」的**纯同步**函数包成节点
```

`fn_node` 是给那些不需要 `await` 的纯函数用的（省掉手写 `Box::pin`）；要 await 的
仍得自己装箱。

## 8.5 `workflows/triage.rs`：前门

**目的**：让「你好」这种闲聊不走完整 loop（那要花两次模型调用：检索门 + 主模型），
而是走一次小模型直答。**图只能让这一轮更快，绝不能让它更差**——任何一步出问题都
掉回完整路径。

### 图结构

```
START ─┬─→ classify      （llm：判 quick / full）
       └─→ check_calendar（tool：读 calendar.ics）

classify ─────┐
              ├─→ gather  （fn：什么都不做，只等两条并行分支到齐）
check_calendar┘
                    │
                    ├─ router: key_router("route", "full")
                    │     quick → quick_reply ─→ END
                    │     full  → full_agent  ─→ END
```

为什么需要一个空的 `gather` 节点：路由器只在**一个**节点上挂，所以需要一个汇合点；
直接挂 `classify` 会在 `check_calendar` 还没跑完时就决策。空节点是这个引擎里
表达「join」的方式。

### 两个提示词

- **`TRIAGE_PROMPT`**（classify）：只回 `{"route":"quick"|"full","reason":"<5 words>"}`；
  定义 quick = 问候/致谢/确认/纯闲聊，full = 涉及任务/日程/人/笔记/记忆或需要工具，
  **拿不准选 full**。
- **`QUICK_REPLY_PROMPT`**（quick_reply）：人设是「Joy，温暖简洁的助手」，回一两句
  自然短句；内含今天的日历与用户消息。

### 失败开放的四条分流（`classify_message`）

```
Err          → ("full", "triage failed open (ProviderError)")
没有 JSON     → ("full", "no JSON — failing open")
JSON 坏了     → ("full", "triage failed open (ParseError)")
route 不认识  → ("full", "bad route '{x}' — failing open")
```

每条都带上**为什么**降级，它会原样进 `GraphInfo.reason` 与 `TurnMeta.graph`——
用户能看到「这次为什么走了完整路径」。

### 注入的闭包（这一层的可测性来源）

```rust
ClassifyFn = Arc<dyn Fn(String) -> Boxed<(String, String)>>   // 真实实现调小模型，测试塞桩
CalendarFn = Arc<dyn Fn() -> String>                          // todays_events 读 calendar.ics
QuickFn    = Arc<dyn Fn(State) -> Boxed<String>>
FullFn     = Arc<dyn Fn(NodeCtx) -> Boxed<NodeWrites>>        // 就是不带图时的同一个 full_turn
```

`todays_events(home)` 是一个极简的 ICS 读取：逐行找 `SUMMARY:` 记标题、
`DTSTART` 含今天且标题非空就收；空 → `"(nothing today)"`。它被 triage 与
`joy gather` 共用——「今天有什么」只有**一个**解析器，两张图才不会打架。

`triage_topology()` 用桩建图后 `describe()`，**从不运行**：拓扑即数据，给驾驶舱
画图用。

## 8.6 `workflows/gather.rs`：晨报

### 两条铁律（模块注释，值得逐字读）

1. **只提议，绝不行动**：全图**没有 agent 节点**、没有 loop、没有 ToolRegistry；
   唯一的模型调用是裸 `create` 且 **`tools: []`**；全图唯一的写是 outbox 里一个
   给人看的 markdown 文件。
2. **每条分支自己兜住自己的失败**：抛错的节点不触发任何边，所以 scan 失败时返回
   **诚实文字** `unavailable(why)`（"unavailable (JOY_GH_REPO 没设…)"）而不是 Err。
   早餐简报最坏的失败方式是**沉默**——某一段空着，人不知道是「今天没有」还是
   「脚本坏了」。

### 图结构

```
START ─┬─→ scan_github   ─┐
       ├─→ scan_web      ─┤
       ├─→ scan_calendar ─┼─→ synthesize ─ router: needs_action ─┬─ propose → draft_digest → END
       └─→ scan_memory   ─┘                                     └─ quiet   → END
```

四路 scan 互不依赖 → 同波并行；`synthesize` 本身就是汇合点（所以这里不需要空
join 节点）。

### 每路 scan 写的键（`scan_owns` 是它的清单）

| 节点 | 成功 | 失败 |
|---|---|---|
| `scan_github` | `gh_text`、`gh_open_prs`、`gh_open_issues` | `gh_text=unavailable(…)`、两个计数 `0` |
| `scan_web` | `web_text` | `web_text=unavailable(…)` |
| `scan_calendar` | `cal_text`、`cal_event_count` | 同上 |
| `scan_memory` | `mem_text` | 同上 |

`scan_owns` 存在是为了让测试能断言「同波并行节点写不相交的键」——那是上面那条
`Collision` 检查成立的前提。

### 路由只看计数，不看散文

```rust
fn needs_action(state) -> String {
    if gh_open_prs > 0 || gh_open_issues > 0 || cal_event_count > 0 { "propose" } else { "quiet" }
}
```

**刻意不对摘要的措辞路由**：让模型决定控制流是不可测的；计数是精确的、便宜的、
可断言的。「今天没什么事」这条路径因此完全不写文件。

### 唯一的模型调用：`synth_request(model, state)`

`DIGEST_PROMPT` 有四个槽位（`{gh_text}` `{web_text}` `{cal_text}` `{mem_text}`），
要求写一份 markdown 简报：先两三件要紧事并说明为什么，再按组列等待维护者处理的
事项，最后一句今日建议焦点。提示词里有一句关键的自我限制：

> 你在**起草提案**，没做也做不了任何事，绝不写成已回复/已合并/已发送。

`tools: Vec::new()` 是「无 tools 铁律」的最后一道关口——即使有人改了别处，
这一层也不会让晨报有机会动手。

### `draft_digest`

写 `<home>/outbox/gather-{日期}.md`，返回路径。这是全图唯一的副作用。
`gather_topology()` 同样用桩建图、从不运行。
