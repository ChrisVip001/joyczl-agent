# ⑤ agent 循环：`joyczl-loop`

整个项目的心脏，也是最薄的一层（`lib.rs` 392 行 + 测试 422 行）。它只回答一个问题：
**拿着工作记忆，反复「问模型 → 执行工具 → 把结果喂回去」，什么时候停。**

目录：`joy-rs/joyczl-loop/src/`（`lib.rs` + `loop_tests.rs`）。

## 5.1 `Turn`：一次循环的全部输入

```rust
pub struct Turn<'a> {
    client: &'a dyn Provider,      // 模型（trait object，测试可换 Mock）
    model: &'a str,
    system: String,                // 已拼好的 system prompt（SOUL+时间+记忆+技能+摘要）
    history: Vec<Message>,         // 最近 N 轮（滑窗），run 内会追加本轮用户消息
    user_message: String,
    tools: &'a ToolRegistry,
    ctx: ToolCtx,                  // 工具执行环境（facts/episodes/chat/calendar/home）
    max_iterations: i32,
    max_tokens: i32,
    observer: Option<Observer>,    // 事件出口：给 trace / 通知 / 图引擎
    on_text: Option<TextSink>,     // 流式文本出口：给界面
    interrupt: Option<Arc<Interrupt>>,  // 取消令牌
}
```

`history` 与 `user_message` 分开而不是「history 里已经含最后一条」：调用方
（`app-server`）从数据库重建历史，本轮用户消息来自请求，两者来源不同，
混在一起容易在重试/重放时出错。

## 5.2 事件：`LoopEvent` 与两个出口的区别

```rust
pub enum LoopEvent {
    Llm  { iteration, stop_reason, usage },   // 每次模型调用完成后
    Text { delta },                           // 流式增量
    ToolStart { name, args },                 // 工具开始执行前
    Tool    { name, args, output },           // 工具执行完成后
}
type Observer = Arc<dyn Fn(LoopEvent) + Send + Sync>;
```

**`observer` 与 `on_text` 是两个不同的通道**，初学者容易混：

| | `on_text` | `observer` |
|---|---|---|
| 谁用 | 界面（REPL/dashboard 的流式渲染） | 服务端（`ToolStarted` 通知）、trace、图引擎 |
| 收什么 | 只有文本增量 | 4 种事件 |
| 没有会怎样 | provider 退回非流式 | 什么都不发，逻辑照跑 |

`Text` 事件与 `on_text` 是**同一份 delta 的两个去向**（⑤.4 里四种组合的合并闭包
说的就是这件事）。

## 5.3 `Interrupt`：两根旗杆，不用 CancellationToken

```rust
pub struct Interrupt { cancelled: AtomicBool, notify: tokio::sync::Notify }
cancel()       -> store(true) + notify_waiters()      // 幂等
is_cancelled() -> load()
wait()         -> while !cancelled { let n = notified(); if cancelled { return } n.await }
```

`wait()` 的写法是防**丢唤醒**的标准姿势：先注册 `notified()` 再二次检查，
否则「检查通过 → 还没来得及注册 → 对方通知已发完」会永远挂住。

不用 `tokio_util::CancellationToken` 的理由写在注释里：一个原子位加一个 Notify
就够了，不值得为两根旗杆拉进一整个依赖。

## 5.4 `run()` 的完整控制流

```
初始化：
  notify = observer
  messages = history + [user_text(user_message)]
  tool_calls = []、usage = default
  streamed = Arc<Mutex<String>>        ← 打断时的「遗言」缓冲
  record  = |delta| streamed.push_str(delta)

for iteration in 1..=max_iterations.max(1):

  安全点 A（轮首）：已取消 → 立即 interrupted_result(..., iteration-1, ...)

  组装 CreateRequest{model, system, messages.clone(), tools.schemas(), max_tokens}

  按 (on_text, observer) 四种组合构造 future：          ← 见下表
     四条路最终都归约为 Result<CreateResponse>

  安全点 B（模型调用）：有 interrupt 时
     select! { _ = cancel.wait() => return interrupted_result(…) ,
               response = fut   => response? }
     没 interrupt 就直接 await
     输掉的 future 被丢弃 → HTTP 连接关闭（不会在后台继续烧 token）

  usage += response.usage；发 Llm 事件
  messages.push(assistant 的 content)

  护栏 1：response.tool_uses() 为空 → 正常返回 LoopResult{interrupted:false}

  act/observe：对每个 (id, name, input)：
      发 ToolStart
      安全点 C（工具执行）：已取消 → interrupted_result
                            否则 select! { cancel → 打断 , output = tools.execute(…) }
      计时 duration_ms
      发 Tool{output}
      tool_calls.push(ToolOutcome)
      results.push(ContentBlock::ToolResult{tool_use_id: id, content: output})
  messages.push(Message{role: User, content: results})     ← 工具结果作为一轮 User

护栏 2：循环跑满 → reply = "（我还没做完就到了迭代上限——试试把请求拆小一点。）"
```

**四种流式组合**（`:198-229`），这张表解释了「为什么流式与非流式的后续逻辑完全
一样」：

| `on_text` | `observer` | 造的 sink | 调 |
|---|---|---|---|
| 有 | 有 | `record + observer(Text) + 用户 sink` | `client.stream` |
| 有 | 无 | `record + 用户 sink` | `client.stream` |
| 无 | 有 | `record + observer(Text)` | `client.stream` |
| 无 | 无 | —— | `client.create` |

`record` 永远挂在最前面（它保证 `streamed` 里始终有全量文本），所以**即使不传
`on_text`，打断时也能把已经吐出来的半截话救回来**。

## 5.5 打断：三个安全点，一处收兵

| 安全点 | 位置 | 检查方式 | 已完成的迭代数 |
|---|---|---|---|
| A | 轮首 | 纯 `is_cancelled()` | `iteration - 1` |
| B | 模型调用 | `select!` 竞速 | `iteration - 1`（本次调用作废） |
| C | 工具执行 | 先 `is_cancelled()` 再 `select!` | `iteration - 1` |

三处都调 `interrupted_result(...)`：

```rust
fn interrupted_result(streamed, tool_calls, iterations, usage, messages) -> LoopResult {
    let partial = streamed.lock().clone();
    let reply = if partial.trim().is_empty() { "（这轮被打断了。）" } else { partial };
    LoopResult { reply, tool_calls, iterations, usage, messages, interrupted: true }
}
```

**打断不是错误**：返回 `Ok(LoopResult { interrupted: true, .. })`。理由是这个
`LoopResult` 会被上层原样落库（`chat_log`），断了就是断了——半句话加一个标记，
比一个「连接被取消」的异常诚实得多。已经拿到的 `tool_calls` 也一并带回去（工具
真的执行过，历史里就该有）。

**打断的边界**（写在 `docs/limitations.md`）：只有 loop 内部的模型调用与工具执行
可取消；loop 之前的检索门与压缩摘要调用不在竞速范围内，那期间打断要等 loop
开始才生效。

## 5.6 双护栏与「不完美但诚实」的收尾

| 护栏 | 条件 | 结果 |
|---|---|---|
| 1 | 模型不再要工具（`tool_uses()` 为空） | 正常返回，`reply = response.text()` |
| 2 | 迭代次数用满 | `reply` 是一句**说明**：「我还没做完就到了迭代上限——试试把请求拆小一点。」 |

护栏 2 刻意不返回「最后一条模型文本」：那句通常是「我接下来要……」而不是答案，
把它当答案交出去是撒谎。宁可说清楚「没做完」。

## 5.7 `LoopResult` 与 `ok()`

```rust
struct LoopResult { reply, tool_calls: Vec<ToolOutcome>, iterations, usage, messages, interrupted }
struct ToolOutcome { name, output, duration_ms }
impl ToolOutcome { fn ok(&self) -> bool { !self.output.starts_with("Error:") } }
```

- `messages` 是**完整工作记忆**（assistant 的想法、工具调用、工具结果全在），
  给 trace 用。
- `ok()` 是「错误作为文本」约定的报告侧：工具层把失败写成以 `Error:` 开头的
  文本（⑥），这里反过来用它判断成败。两半合起来才是完整机制——所以**改
  `execute` 的错误前缀会静默改掉所有工具的成功率统计**，这是这套约定最脆的地方。

## 5.8 测试钉住了什么

`loop_tests.rs` 覆盖：

- 双护栏各一条（工具往返后正常收尾 / 卡在迭代上限时的文案）；
- 事件顺序：`llm#1=tool_use → tool_start:save_note → tool:save_note → llm#2=end_turn`
  （顺序错就等于 `ToolStarted` 通知会晚于完成通知）；
- 流式增量只来自有文本的那一轮（工具轮的 delta 不该出现）；
- 打断三条：**预先取消**（一次模型调用都不该发生，断言 `received` 为空）、
  **工具往返之间取消**（第一个工具真的执行过，第二轮模型调用没有发生，
  `iterations == 1`）。

**这一层的不变量**：无论如何退出都返回 `Ok(LoopResult)`；打断带 `interrupted: true`
与半截文本；每次模型调用的 usage 累加；工具结果一定以 `ToolResult` 形式回到
工作记忆；没有 `on_text` 也能打断。
