//! THE LOOP — observe → reason → act → repeat. This file is the whole trick.
//!
//! Every agent framework, stripped to the bone, is this loop plus more indirection:
//!
//! ```text
//! while not done:
//!     response = llm(messages, tools)     # reason
//!     if response wants tools:
//!         results = run(tool_calls)       # act
//!         messages += results             # observe
//!     else:
//!         done                            # reply to the human
//! ```
//!
//! Two guardrails (exit conditions):
//!   1. the model stops requesting tools → the turn ends naturally
//!   2. `max_iterations` reached → **one wrap-up call with the tools taken away**, so the turn
//!      ends with "what I did / where I am stuck / what I need", not with a canned apology
//!
//! The observer is an enum of events, not a `(kind, dict)` — fields have types,
//! so a misspelled key is a compile-time error.

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

/// The turn's interrupt switch. `turn/interrupt` flips it down from another task;
/// the loop stands down at the next safe point (model call, or tool execution racing a select).
///
/// No tokio_util CancellationToken: one atomic bit plus a Notify is enough — not worth
/// pulling in a whole dependency for one flagpole.
#[derive(Debug, Default)]
pub struct Interrupt {
    cancelled: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

impl Interrupt {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Flip the switch down. Flipping it again after cancel is idempotent.
    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Returns at the moment of cancellation. Suspends waiting for the notify otherwise.
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

/// Words the human added **while this turn was already running**.
///
/// The loop folds them into the working memory at the next iteration boundary, so tools that
/// already ran stay run and their results stay valid — that is the whole difference between
/// steering and "cancel and start over". One list, two readers: the loop consumes what it has
/// not seen yet (`drain_pending`), app-server reads `all` afterwards to write the transcript,
/// so a message accepted right before the turn ended is still recorded rather than lost.
#[derive(Debug, Default)]
pub struct Steering {
    messages: Vec<String>,
    /// How many of `messages` the loop has already folded in. A counter, not `remove(0)`:
    /// app-server must still see the earlier ones when it writes the log.
    consumed: usize,
}

impl Steering {
    pub fn new() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self::default()))
    }

    /// Accept one addition. Called from another task (`turn/steer`), hence `&mut`.
    pub fn push(&mut self, text: impl Into<String>) {
        self.messages.push(text.into());
    }

    /// Everything not yet folded into the working memory, in arrival order.
    pub fn drain_pending(&mut self) -> Vec<String> {
        let pending = self.messages[self.consumed..].to_vec();
        self.consumed = self.messages.len();
        pending
    }

    /// Everything ever accepted for this turn — the transcript writes all of it.
    pub fn all(&self) -> Vec<String> {
        self.messages.clone()
    }
}

/// Events the loop sends out. The gateway renders them, trace persists them —
/// neither needs to be wired into the loop's logic.
#[derive(Debug, Clone, PartialEq)]
pub enum LoopEvent {
    /// One model call completed.
    Llm {
        iteration: i32,
        stop_reason: StopReason,
        usage: Usage,
    },
    /// One delta of streamed text. Only appears when `on_text` is passed.
    Text { delta: String },
    /// One tool **started** (result not out yet — the Tool event comes later).
    ToolStart { name: String, args: Value },
    /// One tool finished (output is exactly the text the model will see).
    Tool {
        name: String,
        args: Value,
        output: String,
    },
}

/// Observer: every loop event passes through it. Arc instead of a reference,
/// so it can be moved together with `on_text` into a 'static closure.
pub type Observer = Arc<dyn Fn(LoopEvent) + Send + Sync>;

/// One boundary snapshot: the working memory as the model is about to be called again —
/// assistant thoughts, tool calls and their results all in `messages`, nothing half-done.
#[derive(Debug, Clone)]
pub struct Checkpoint {
    pub messages: Vec<Message>,
    /// Completed iterations so far (the tool round just fed back included).
    pub iterations: i32,
    /// Token usage spent so far (a resumed turn adds both halves up).
    pub usage: Usage,
}

/// The checkpoint outlet, called at each tool-results boundary. Awaiting on purpose:
/// the write must land **before** the next model call, or a crash loses exactly the
/// boundary it was supposed to save.
pub type CheckpointSink = Arc<
    dyn Fn(Checkpoint) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// Resume from a checkpoint: the loop continues an existing working memory instead of
/// assembling history + user message — finished tools are never re-run.
#[derive(Debug, Clone)]
pub struct Resume {
    pub messages: Vec<Message>,
    /// How many iterations were already completed before the crash.
    pub completed_iterations: i32,
    /// Token usage already spent before the crash.
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    pub name: String,
    pub output: String,
    pub duration_ms: i64,
}

impl ToolOutcome {
    /// Output starting with "Error:" means this call failed.
    /// This is the reporting side of the "errors as text" convention.
    pub fn ok(&self) -> bool {
        !self.output.starts_with("Error:")
    }
}

#[derive(Debug, Clone)]
pub struct LoopResult {
    /// What the model says to the user.
    pub reply: String,
    pub tool_calls: Vec<ToolOutcome>,
    pub iterations: i32,
    /// Token usage for the whole turn (summed over every LLM call).
    pub usage: Usage,
    /// The full working memory — assistant thoughts, tool calls, tool results all in here;
    /// exactly what trace wants.
    pub messages: Vec<Message>,
    /// True when interrupted by `turn/interrupt`: reply holds text already streamed before
    /// the interrupt, not guaranteed complete.
    pub interrupted: bool,
    /// This turn's loop-guard tally (hit count, last note).
    pub guard: GuardReport,
    /// The **locally estimated** input tokens of the last request (0 = none sent yet).
    /// Only paired with the provider-reported `usage` to calibrate the next turn — not the ledger.
    pub estimated_input_tokens: usize,
    /// The **measured** input tokens of the last call (provider-reported prefill).
    /// Two numbers for the same request as `estimated_input_tokens`: dividing gives the error.
    pub observed_input_tokens: usize,
}

/// What the model is told on its last allowed iteration, once the tools are gone.
///
/// Written as an instruction the model can *follow*, not as an apology: three sentences, no
/// pretending, no invented results. The failure this answers was a turn that spent its whole
/// budget and then handed the human 「试试把请求拆小一点」.
const WRAP_UP: &str = "（预算提示：这是本轮的最后一次模型调用，工具已经收起。）\
用三句话收尾：已经完成的、卡在哪、下一步需要我做什么。\
没查证的事直接说不确定，不要编造结果。";

pub struct Turn<'a> {
    pub client: &'a dyn Provider,
    pub model: &'a str,
    /// The **stable prefix** of the system prompt: soul, model identity, the method
    /// paragraph. Identical turn after turn within a session, so the provider's prompt
    /// cache can serve it at a fraction of the input price (Anthropic needs the explicit
    /// `cache_control` boundary; the wires with automatic caching just need it not to move).
    /// `None` = one undivided system prompt.
    pub system_prefix: Option<String>,
    /// The per-turn tail: clock (day granularity), todo list, memory hits, skill bodies,
    /// the rolling summary. It changes every turn by nature, so it stays outside the cache
    /// boundary — marking it would invalidate the prefix on every write.
    pub system: String,
    /// History from previous turns (sliding-window trimmed). The new message is appended at the end.
    pub history: Vec<Message>,
    pub user_message: String,
    /// Image blocks riding on this turn's user message (attachments, already read and
    /// base64'd by app-server). Empty = plain text turn, zero overhead. They ride only
    /// on the **first** user message — later rounds of the same turn carry tool results.
    pub user_images: Vec<ContentBlock>,
    pub tools: &'a ToolRegistry,
    pub ctx: ToolCtx,
    pub max_iterations: i32,
    pub max_tokens: i32,
    /// Budget for intra-turn tool results (off by default; see `budget.rs`).
    pub tool_result_budget: ToolResultBudget,
    /// Every event passes through it — the gateway renders, trace persists.
    /// Arc instead of a reference, so it can move with `on_text` into a 'static closure.
    pub observer: Option<Arc<dyn Fn(LoopEvent) + Send + Sync>>,
    /// Pass it for streaming: one callback per text chunk the model emits.
    /// Runs fine without it — the provider falls back to non-streaming.
    pub on_text: Option<TextSink>,
    /// Pass it to support interrupts: each model call and tool execution races "cancel".
    pub interrupt: Option<Arc<Interrupt>>,
    /// Called at each tool-results boundary — the durable "how far did this turn get".
    pub checkpoint: Option<CheckpointSink>,
    /// Adds words to this turn mid-flight (`turn/steer`). `None` = no steering channel.
    pub steering: Option<Arc<Mutex<Steering>>>,
    /// Resume instead of starting fresh: `user_message`, `history` and `user_images` are
    /// ignored, the loop continues from the checkpointed working memory.
    pub resume: Option<Resume>,
}

pub async fn run(turn: Turn<'_>) -> Result<LoopResult> {
    let notify: Option<Observer> = turn.observer.clone();
    // Fresh start or resume: a resumed turn skips assembly entirely — history, user message
    // and images were already folded into the checkpointed working memory before the crash.
    let (mut messages, mut usage, completed_before) = match &turn.resume {
        Some(resume) => (
            resume.messages.clone(),
            resume.usage,
            resume.completed_iterations,
        ),
        None => {
            let mut messages = turn.history.clone();
            let mut user_content = vec![ContentBlock::Text {
                text: turn.user_message.clone(),
            }];
            user_content.extend(turn.user_images.iter().cloned());
            messages.push(Message {
                role: Role::User,
                content: user_content,
            });
            (messages, Usage::default(), 0)
        }
    };
    let mut tool_calls: Vec<ToolOutcome> = Vec::new();
    // Spill directory: `ToolCtx` already carries home, no extra parameter to thread through.
    let spill_dir = turn.ctx.home.join("spill");
    // Last request's local estimate + measured value (app-server calibrates estimate with measured).
    let mut last_estimate = 0usize;
    let mut last_observed = 0usize;
    // Within-turn loop detection (cross-turn repetition is app-server's fold_tool_activity's job).
    let mut guard = StallGuard::new();

    // All text the model streams during this turn. On interrupt, this is the part of the reply
    // that can be rescued — half a sentence cut mid-way is more honest than a flat "gone".
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

    for iteration in (completed_before + 1)..=turn.max_iterations.max(1) {
        // ---- Interrupt check: token already flipped down means no new model call; stand down now.
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

        // ---- Steering: words added while this turn was running. They join the working memory
        // as ordinary user messages — at this boundary, so the model sees them on the next call
        // while every tool result already in `messages` stays untouched.
        if let Some(steering) = &turn.steering {
            let pending = steering
                .lock()
                .expect("steering 锁不该中毒")
                .drain_pending();
            for text in pending {
                messages.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text { text }],
                });
            }
        }

        // ---- The last allowed iteration is a **wrap-up**, not a failure.
        //
        // Claude Code's shape, borrowed deliberately: its interactive loop has no turn cap at
        // all — it ends when the model stops calling tools, and the budget that bites is the
        // context window (auto-compact). We keep a cap (a local model on a laptop should not be
        // able to spend a night), but the cap must not be a cliff: before the budget runs out,
        // the tools are taken away and the model is asked to say what it did.
        //
        // What this replaces: a hard stop whose whole answer was 「我还没做完就到了迭代上限」
        // — after 10 calls and 210k input tokens, the human learned nothing they could act on.
        let wrapping_up = iteration == turn.max_iterations && turn.max_iterations > 1;
        if wrapping_up {
            messages.push(Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: WRAP_UP.to_string(),
                }],
            });
        }

        // ---- Intra-turn tool-result budget: stub oversized results before assembling the request.
        //
        // Placed here rather than after tool execution: stubbing should only affect the **text
        // sent to the model**, and every turn may accumulate more results, so one sweep before
        // the request is simplest (the function is idempotent; existing stubs are skipped).
        budget::trim_tool_results(
            &mut messages,
            Some(&spill_dir),
            &turn.tool_result_budget,
            &turn.ctx.session_id,
        );

        // ---- reason: call the model once with the current working memory
        // No tools on the wrap-up call: the point is a summary, and a tool call there would
        // buy another round the budget no longer has.
        let schemas = if wrapping_up {
            Vec::new()
        } else {
            turn.tools.schemas()
        };
        // Estimate = messages + system + tool schemas. Paired with the provider-reported usage
        // to calibrate the next turn's budget (a skewed estimate only shifts compaction earlier/later).
        last_estimate = joyczl_provider::tokens::estimate_messages(&messages)
            + joyczl_provider::tokens::estimate_text(&turn.system)
            + turn
                .system_prefix
                .as_deref()
                .map(joyczl_provider::tokens::estimate_text)
                .unwrap_or(0)
            + joyczl_provider::tokens::estimate_tools(&schemas);
        let request = CreateRequest {
            cache_prefix: turn.system_prefix.clone(),
            model: turn.model.to_string(),
            system: Some(turn.system.clone()),
            messages: messages.clone(),
            tools: schemas,
            max_tokens: turn.max_tokens,
        };

        // With a streaming sink, go streaming: text deltas go both to on_text (UI) and as Text
        // events to the observer (trace), plus a copy into streamed (last words on interrupt).
        // Without a streaming sink, fall back to a one-shot call — the rest is identical.
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

        // The model call races "cancel": whoever arrives first wins. The losing future is
        // dropped and its HTTP connection closed — no tokens burned in the background.
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

        // This turn's assistant message (text and/or tool requests) enters working memory
        messages.push(Message {
            role: Role::Assistant,
            content: response.content.clone(),
        });

        let uses = response.tool_uses();

        // ---- Guardrail 1: no tools requested → the model is talking to the human
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

        // ---- act: run each requested tool; observe: feed the results back
        let mut results = Vec::new();
        for (id, name, input) in uses {
            // Tool starting. Sent before the result — so the client can render "calling X".
            if let Some(notify) = &notify {
                notify(LoopEvent::ToolStart {
                    name: name.to_string(),
                    args: input.clone(),
                });
            }

            let started = Instant::now();
            // Tool execution also races "cancel": a half-run tool future is dropped, and its
            // child processes/connections are cleaned up by their own Drop impls.
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
            // The guard takes a look: on a hit, stub + append a reminder. **Only the fed-back
            // text changes** — tool_calls and Tool events keep the real result.
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

        // ---- Checkpoint: this boundary is durable **before** the next model call goes out.
        // Crash here = resume from exactly here; the finished tool round is never re-run.
        if let Some(checkpoint) = &turn.checkpoint {
            checkpoint(Checkpoint {
                messages: messages.clone(),
                iterations: iteration,
                usage,
            })
            .await;
        }
    }

    // ---- Guardrail 2: iterations exhausted (the wrap-up call is the normal exit; this is the
    // residue: `max_iterations == 1`, or a provider that keeps emitting tool calls with an empty
    // tool table). Say what happened and how to continue — never "ask smaller".
    Ok(LoopResult {
        reply: format!(
            "（这一轮用完了 {} 步预算，没能收尾。上面每步的工具调用与结果都还在，\
             说一声「接着做」我就从断的地方继续；想一次做完的话，也可以把它拆成两步说。）",
            turn.max_iterations
        ),
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

/// The stand-down result on interrupt: reply is text already streamed (or one explanatory line),
/// and `interrupted: true` lets the caller persist it honestly, not pass off half an answer.
/// The stand-down scene. Both interrupt and "iteration cap" assemble `LoopResult` here — past
/// seven or eight params, a struct reads better, and adding a field no longer breaks four sites.
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

/// An empty observer, saving callers a closure when they need no events.
pub fn noop_observer(_: LoopEvent) {}

#[cfg(test)]
#[path = "loop_tests.rs"]
mod loop_tests;
