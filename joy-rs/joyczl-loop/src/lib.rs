//! THE LOOP —— observe → reason → act → repeat。这个文件就是全部技巧。
//!
//! 每个 agent 框架，剥到底都是这个循环加更多间接层：
//!
//! ```text
//! while not done:
//!     response = llm(messages, tools)     # reason
//!     if response 想调工具:
//!         results = run(tool_calls)       # act
//!         messages += results             # observe
//!     else:
//!         done                            # 回复人类
//! ```
//!
//! 两个护栏（出口条件）：
//!   1. 模型不再要工具   → 一轮自然结束
//!   2. 到达 max_iterations → 硬停，绝不空转
//!
//! observer 是一个枚举事件而不是 `(kind, dict)` —— 字段有了类型，
//! 拼错 key 编译期就报错。

use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use joyczl_provider::{
    ContentBlock, CreateRequest, Message, Provider, Role, StopReason, TextSink, Usage,
};
use joyczl_tools::{ToolCtx, ToolRegistry};
use serde_json::Value;

pub mod budget;
pub mod guard;

use budget::ToolResultBudget;
use guard::{GuardReport, StallGuard};

#[cfg(test)]
#[path = "guard_tests.rs"]
mod guard_tests;

#[cfg(test)]
#[path = "budget_tests.rs"]
mod budget_tests;

/// 一轮 turn 的打断开关。`turn/interrupt` 在别的任务里把它拨下去，
/// loop 在下一个安全点（模型调用、工具执行的途中以 select 竞速）收兵。
///
/// 不用 tokio_util 的 CancellationToken：一个原子位加一个 Notify 就够了，
/// 不值得为一根旗杆拉进一整个依赖。
#[derive(Debug, Default)]
pub struct Interrupt {
    cancelled: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

impl Interrupt {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 拨下开关。已经取消时再拨是幂等的。
    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 取消的那一刻返回。没取消就挂起等通知。
    pub async fn wait(&self) {
        while !self.is_cancelled() {
            let notified = self.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// loop 往外发的事件。gateway 拿它画界面，trace 拿它落盘 ——
/// 两者都不需要被接进 loop 的逻辑里。
#[derive(Debug, Clone, PartialEq)]
pub enum LoopEvent {
    /// 一次模型调用完成。
    Llm {
        iteration: i32,
        stop_reason: StopReason,
        usage: Usage,
    },
    /// 流式文本的一个增量。只在传了 `on_text` 时出现。
    Text { delta: String },
    /// 一次工具**开始**执行（结果还没出来 —— ToolCompleted 事件在后面）。
    ToolStart { name: String, args: Value },
    /// 一次工具执行完成（output 就是模型会看到的那段文本）。
    Tool {
        name: String,
        args: Value,
        output: String,
    },
}

/// 观察者：每个 loop 事件都会经过它。用 Arc 而不是引用，
/// 是为了能和 `on_text` 一起被搬进 'static 的闭包里。
pub type Observer = Arc<dyn Fn(LoopEvent) + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    pub name: String,
    pub output: String,
    pub duration_ms: i64,
}

impl ToolOutcome {
    /// 输出以 "Error:" 开头 = 这次调用失败了。
    /// 这就是「错误作为文本」约定在报告侧的另一半。
    pub fn ok(&self) -> bool {
        !self.output.starts_with("Error:")
    }
}

#[derive(Debug, Clone)]
pub struct LoopResult {
    /// 模型对用户说的话。
    pub reply: String,
    pub tool_calls: Vec<ToolOutcome>,
    pub iterations: i32,
    /// 整轮的 token 用量（每次 LLM 调用累加）。
    pub usage: Usage,
    /// 完整的工作记忆 —— assistant 的想法、工具调用、工具结果全在里面，
    /// trace 要的就是这个。
    pub messages: Vec<Message>,
    /// 被 `turn/interrupt` 打断时为 true：reply 里是打断前已经吐出的文本，
    /// 不保证完整。
    pub interrupted: bool,
    /// 这一轮里循环护栏的账（命中几次、最后说了什么）。
    pub guard: GuardReport,
    /// 最后一次请求的**本地估算**输入 token 数（0 = 还没发过请求）。
    /// 只用来与 provider 回报的 `usage` 配对，做下一轮的校准 —— 它本身不是账。
    pub estimated_input_tokens: usize,
    /// 最后一次调用的**实测**输入 token（provider 回报的 prefill）。
    /// 与 `estimated_input_tokens` 是同一个请求的两个数字：相除就是估算的偏差。
    pub observed_input_tokens: usize,
}

pub struct Turn<'a> {
    pub client: &'a dyn Provider,
    pub model: &'a str,
    pub system: String,
    /// 之前几轮的历史（滑窗截过的）。函数内部会把这条新消息追加在末尾。
    pub history: Vec<Message>,
    pub user_message: String,
    pub tools: &'a ToolRegistry,
    pub ctx: ToolCtx,
    pub max_iterations: i32,
    pub max_tokens: i32,
    /// 轮内工具结果的预算（默认关；见 `budget.rs`）。
    pub tool_result_budget: ToolResultBudget,
    /// 每个事件都会经过它 —— gateway 拿它画界面，trace 拿它落盘。
    /// 用 Arc 而不是引用，是为了能和 `on_text` 一起被搬进 'static 闭包。
    pub observer: Option<Arc<dyn Fn(LoopEvent) + Send + Sync>>,
    /// 传了就走流式：模型每吐一段文本就回调一次。
    /// 没传也能跑 —— provider 会退回非流式。
    pub on_text: Option<TextSink>,
    /// 传了就支持打断：每个模型调用和工具执行都跟「取消」竞速。
    pub interrupt: Option<Arc<Interrupt>>,
}

pub async fn run(turn: Turn<'_>) -> Result<LoopResult> {
    let notify: Option<Observer> = turn.observer.clone();
    let mut messages = turn.history.clone();
    messages.push(Message::user_text(&turn.user_message));

    let mut tool_calls: Vec<ToolOutcome> = Vec::new();
    let mut usage = Usage::default();
    // 落盘目录：`ToolCtx` 已经有 home，不用再穿一层参数。
    let spill_dir = turn.ctx.home.join("spill");
    // 最后一次请求的本地估算 + 实测（给 app-server 做「用实测校准估算」）。
    let mut last_estimate = 0usize;
    let mut last_observed = 0usize;
    // 本轮内的循环检测（跨轮的重复由 app-server 的 fold_tool_activity 负责）。
    let mut guard = StallGuard::new();

    // 流式期间模型吐出的全部文本。被打断时，这就是 reply 里能救回来的部分 ——
    // 断在句中间的半句话，也比一句"没了"诚实。
    let streamed: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let record: TextSink = {
        let streamed = streamed.clone();
        Arc::new(move |delta: &str| {
            streamed
                .lock()
                .expect("streamed 锁不该中毒")
                .push_str(delta);
        })
    };

    for iteration in 1..=turn.max_iterations.max(1) {
        // ---- 打断检查：取消令牌已拨下就不发起新的模型调用，立刻收兵。
        if turn
            .interrupt
            .as_ref()
            .is_some_and(|cancel| cancel.is_cancelled())
        {
            return Ok(interrupted_result(Scene {
                streamed,
                tool_calls,
                iterations: iteration - 1,
                usage,
                messages,
                guard: GuardReport::close(&guard),
                estimated_input_tokens: last_estimate,
                observed_input_tokens: last_observed,
            }));
        }

        // ---- 轮内工具结果预算：组装请求前把太大的结果换成桩。
        //
        // 放在这里而不是工具执行完之后：换桩只该影响**发给模型的文本**，
        // 而每一轮都可能又攒了几条结果，所以在请求前统一过一遍最省心
        // （函数本身幂等，已是桩的直接跳过）。
        budget::trim_tool_results(
            &mut messages,
            Some(&spill_dir),
            &turn.tool_result_budget,
            &turn.ctx.session_id,
        );

        // ---- reason：带着当前工作记忆调一次模型
        let schemas = turn.tools.schemas();
        // 估算 = 消息 + system + 工具声明。与 provider 回报的 usage 配对，
        // 用来校准下一轮的预算（估歪了只会让压缩早/晚发生，不会算错账）。
        last_estimate = joyczl_provider::tokens::estimate_messages(&messages)
            + joyczl_provider::tokens::estimate_text(&turn.system)
            + joyczl_provider::tokens::estimate_tools(&schemas);
        let request = CreateRequest {
            model: turn.model.to_string(),
            system: Some(turn.system.clone()),
            messages: messages.clone(),
            tools: schemas,
            max_tokens: turn.max_tokens,
        };

        // 有流式出口就走流式：文本增量既转发给 on_text（界面），也作为
        // Text 事件给 observer（trace），再抄一份进 streamed（打断时的遗言）。
        // 没有流式出口就退回一次性调用 —— 后面的逻辑完全一样。
        let fut = match (turn.on_text.clone(), notify.clone()) {
            (Some(sink), Some(observer)) => {
                let (record, streamed_sink) = (record.clone(), sink.clone());
                let combined: TextSink = Arc::new(move |delta: &str| {
                    record(delta);
                    observer(LoopEvent::Text {
                        delta: delta.to_string(),
                    });
                    streamed_sink(delta);
                });
                turn.client.stream(request, combined)
            }
            (Some(sink), None) => {
                let (record, record_only) = (record.clone(), sink.clone());
                let chained: TextSink = Arc::new(move |delta: &str| {
                    record(delta);
                    record_only(delta);
                });
                turn.client.stream(request, chained)
            }
            (None, Some(observer)) => {
                let record = record.clone();
                let observer_only: TextSink = Arc::new(move |delta: &str| {
                    record(delta);
                    observer(LoopEvent::Text {
                        delta: delta.to_string(),
                    });
                });
                turn.client.stream(request, observer_only)
            }
            (None, None) => turn.client.create(request),
        };

        // 模型调用与「取消」竞速：谁先到算谁的。输掉的那次调用 future 被
        // 丢弃，HTTP 连接随之关闭 —— 不会在后台继续烧 token。
        let response = match turn.interrupt.as_ref() {
            Some(cancel) => {
                tokio::select! {
                    _ = cancel.wait() => {
                                return Ok(interrupted_result(Scene {
                                    streamed,
                                    tool_calls,
                                    iterations: iteration - 1,
                                    usage,
                                    messages,
                                    guard: GuardReport::close(&guard),
                                    estimated_input_tokens: last_estimate,
                                    observed_input_tokens: last_observed,
                                }));
                    }
                    response = fut => response?,
                }
            }
            None => fut.await?,
        };

        usage.input_tokens += response.usage.input_tokens;
        usage.output_tokens += response.usage.output_tokens;
        last_observed = response.usage.input_tokens.max(0) as usize;
        if let Some(notify) = &notify {
            notify(LoopEvent::Llm {
                iteration,
                stop_reason: response.stop_reason,
                usage: response.usage,
            });
        }

        // assistant 的这轮（文本和/或工具请求）进入工作记忆
        messages.push(Message {
            role: Role::Assistant,
            content: response.content.clone(),
        });

        let uses = response.tool_uses();

        // ---- 护栏 1：不再要工具 → 模型在跟人说话
        if uses.is_empty() {
            return Ok(LoopResult {
                reply: response.text(),
                tool_calls,
                iterations: iteration,
                usage,
                messages,
                interrupted: false,
                guard: GuardReport::close(&guard),
                estimated_input_tokens: last_estimate,
                observed_input_tokens: last_observed,
            });
        }

        // ---- act：逐个执行请求的工具；observe：把结果喂回去
        let mut results = Vec::new();
        for (id, name, input) in uses {
            // 工具开始执行。先于结果发出 —— 客户端才能画出"正在调用 X"。
            if let Some(notify) = &notify {
                notify(LoopEvent::ToolStart {
                    name: name.to_string(),
                    args: input.clone(),
                });
            }

            let started = Instant::now();
            // 工具执行同样与「取消」竞速：跑一半的工具 future 被丢弃，
            // 它拉起的子进程/连接由各自的 Drop 收尾。
            let output = match turn.interrupt.as_ref() {
                Some(cancel) if cancel.is_cancelled() => {
                    return Ok(interrupted_result(Scene {
                        streamed,
                        tool_calls,
                        iterations: iteration - 1,
                        usage,
                        messages,
                        guard: GuardReport::close(&guard),
                        estimated_input_tokens: last_estimate,
                        observed_input_tokens: last_observed,
                    }));
                }
                Some(cancel) => {
                    let fut = turn.tools.execute(turn.ctx.clone(), name, input.clone());
                    tokio::select! {
                        _ = cancel.wait() => {
                                    return Ok(interrupted_result(Scene {
                                        streamed,
                                        tool_calls,
                                        iterations: iteration - 1,
                                        usage,
                                        messages,
                                        guard: GuardReport::close(&guard),
                                        estimated_input_tokens: last_estimate,
                                        observed_input_tokens: last_observed,
                                    }));
                        }
                        output = fut => output,
                    }
                }
                None => {
                    turn.tools
                        .execute(turn.ctx.clone(), name, input.clone())
                        .await
                }
            };
            let duration_ms = started.elapsed().as_millis() as i64;

            if let Some(notify) = &notify {
                notify(LoopEvent::Tool {
                    name: name.to_string(),
                    args: input.clone(),
                    output: output.clone(),
                });
            }
            tool_calls.push(ToolOutcome {
                name: name.to_string(),
                output: output.clone(),
                duration_ms,
            });
            // 护栏过一眼：命中就换桩 + 追加提醒。**只改喂回模型的文本** ——
            // tool_calls 与 Tool 事件里留的都是真结果。
            let verdict = guard.observe(name, input, &output);
            let for_model = guard::for_model(&output, name, input, &verdict);
            results.push(ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: for_model,
            });
        }
        messages.push(Message {
            role: Role::User,
            content: results,
        });
    }

    // ---- 护栏 2：迭代次数用完了
    Ok(LoopResult {
        reply: "（我还没做完就到了迭代上限——试试把请求拆小一点。）".to_string(),
        tool_calls,
        iterations: turn.max_iterations,
        usage,
        messages,
        interrupted: false,
        guard: GuardReport::close(&guard),
        estimated_input_tokens: last_estimate,
        observed_input_tokens: last_observed,
    })
}

/// 打断时的收兵结果：reply 是已经流出来的文本（没有就一句话说明），
/// `interrupted: true` 让上层如实落库，而不是把半截话当成完整回答。
/// 收兵现场。打断与「迭代上限」都从这里拼 `LoopResult` —— 参数超过七八个之后，
/// 一个结构体比一长串实参好读，也少一次「又加了一个字段、四处调用都得改」。
struct Scene {
    streamed: Arc<Mutex<String>>,
    tool_calls: Vec<ToolOutcome>,
    iterations: i32,
    usage: Usage,
    messages: Vec<Message>,
    guard: GuardReport,
    estimated_input_tokens: usize,
    observed_input_tokens: usize,
}

fn interrupted_result(scene: Scene) -> LoopResult {
    let Scene {
        streamed,
        tool_calls,
        iterations,
        usage,
        messages,
        guard,
        estimated_input_tokens,
        observed_input_tokens,
    } = scene;
    let partial = streamed.lock().expect("streamed 锁不该中毒").clone();
    let reply = if partial.trim().is_empty() {
        "（这轮被打断了。）".to_string()
    } else {
        partial
    };
    LoopResult {
        reply,
        tool_calls,
        iterations,
        usage,
        messages,
        interrupted: true,
        guard,
        estimated_input_tokens,
        observed_input_tokens,
    }
}

/// 空的 observer，调用方不需要事件时省得写闭包。
pub fn noop_observer(_: LoopEvent) {}

#[cfg(test)]
#[path = "loop_tests.rs"]
mod loop_tests;
