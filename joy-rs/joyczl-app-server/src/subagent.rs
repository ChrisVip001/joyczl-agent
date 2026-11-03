//! The subagent's executor.
//!
//! It lives at this layer because it needs the model, state handles, and tool table — `joyczl-tools`
//! only knows the `SubagentRunner` trait (the dependency direction cannot be reversed: loop depends on tools).
//!
//! Differences from the parent turn, spelled out one by one:
//!
//! * **Its own context**: empty history + a short system (persona + one line "you do only this task"),
//!   no retrieval, no skills, no summary. A delegated task should be self-contained.
//! * **Its own iteration cap**: default 5, hard cap 10. Allowing dozens of turns amounts to secretly
//!   running a whole long conversation elsewhere, while "saving context" is the very reason for delegation.
//! * **No further delegation**: the tool table explicitly drops `delegate_task` (recursive delegation is
//!   the kind of thing that is invisible until someone tries it and burns through the quota).
//! * **No notifications, no interrupts**: the parent turn only knows "delegate_task was called", then gets the conclusion.
//! * **Nothing persisted**: the subagent's conversation is not written to `chat_log` (otherwise
//!   `session/list` would fill with unclaimed sessions). To know what it did, read the returned summary — it lists its tools.
//!
//! Model and settings are **read at runtime** (sharing those two Arcs, not cloning at construction):
//! a turn arriving after `config/write` switches providers, or after `install_provider` in tests,
//! must use the **current** client — capturing a snapshot from startup is a silent trap.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use joyczl_config::Settings;
use joyczl_provider::Resolved;
use joyczl_state::{Calendar, Chat, Episodes, Facts};
use joyczl_tools::{subagent::SubagentRunner, BoxFut, ToolRegistry};
use serde_json::Value;

/// Default iteration cap: a delegated task should be smaller than a full conversation.
const DEFAULT_ITERATIONS: i32 = 5;
/// Hard cap: beyond this it is no longer "delegation".
const MAX_ITERATIONS: i32 = 10;
/// The subagent's answer should not consume a whole turn's quota: it delivers conclusions.
const SUBAGENT_MAX_TOKENS: i32 = 2048;

pub(crate) struct Delegated {
    /// A copy of the parent turn's tool table (already without `delegate_task`).
    pub(crate) tools: Arc<ToolRegistry>,
    pub(crate) facts: Facts,
    pub(crate) episodes: Episodes,
    pub(crate) chat: Chat,
    pub(crate) calendar: Calendar,
    /// Settings and model handles shared with `Server` (read at runtime; see module docs).
    pub(crate) settings: Arc<RwLock<Settings>>,
    pub(crate) resolved: Arc<RwLock<Option<Resolved>>>,
    /// The subagent's tool calls also go through hooks (tool-level events are fired by `ToolRegistry::execute`).
    pub(crate) hooks: Option<Arc<joyczl_tools::hooks::Hooks>>,
}

/// What gets fed to the subagent this time.
struct Prompt {
    system: String,
    history: Vec<joyczl_provider::Message>,
    user_message: String,
}

/// Run one subagent loop. Extracted because "the structured result failed validation" needs **one
/// more round**, and that round is identical to the first except for history and instruction.
async fn run_child(
    resolved: &Resolved,
    tools: &ToolRegistry,
    ctx: joyczl_tools::ToolCtx,
    prompt: Prompt,
    max_iterations: i32,
    budget: joyczl_loop::budget::ToolResultBudget,
) -> anyhow::Result<joyczl_loop::LoopResult> {
    joyczl_loop::run(joyczl_loop::Turn {
        client: resolved.client.as_ref(),
        model: &resolved.model,
        system: prompt.system,
        history: prompt.history,
        user_message: prompt.user_message,
        tools,
        ctx,
        max_iterations,
        max_tokens: SUBAGENT_MAX_TOKENS,
        tool_result_budget: budget,
        observer: None,
        on_text: None,
        interrupt: None,
    })
    .await
}

/// Squeeze the subagent's conclusion into a "schema-valid JSON string".
fn coerce_structured(reply: &str, schema: &Value) -> Result<String, String> {
    let Some(json) = joyczl_memory::gate::extract_json(reply) else {
        return Err("回复里没找到 JSON 对象".to_string());
    };
    let Ok(value) = serde_json::from_str::<Value>(&json) else {
        return Err("那段 JSON 解析不了".to_string());
    };
    joyczl_tools::validate_value(schema, &value)?;
    serde_json::to_string(&value).map_err(|e| e.to_string())
}

impl SubagentRunner for Delegated {
    fn run(
        &self,
        task: String,
        max_iterations: Option<i32>,
        result_schema: Option<serde_json::Value>,
        parent_cwd: Option<std::path::PathBuf>,
    ) -> BoxFut {
        let tools = self.tools.clone();
        let settings = self.settings.clone();
        let resolved = self.resolved.clone();
        let hooks = self.hooks.clone();
        let facts = self.facts.clone();
        let episodes = self.episodes.clone();
        let chat = self.chat.clone();
        let cwd = parent_cwd;
        let calendar = self.calendar.clone();

        Box::pin(async move {
            // Read now: this turn uses the current provider and settings.
            let Some(resolved) = resolved.read().expect("resolved 锁不该中毒").clone() else {
                return Err(anyhow::anyhow!("模型还没配好，子代理起不来"));
            };
            let settings = settings.read().expect("settings 锁不该中毒").clone();
            let home: PathBuf = settings.home.clone();

            let iterations = max_iterations
                .unwrap_or(DEFAULT_ITERATIONS)
                .clamp(1, MAX_ITERATIONS);
            // The event payload uses a short head of the task: it may be long, and the hook wants "what is being done".
            let task_preview: String = task.chars().take(400).collect();

            let soul = crate::turn::load_soul(&home);
            let system = crate::turn::build_system(
                &soul,
                &resolved.model,
                &resolved.provider_id,
                "",
                "",
                // The subagent has no todo list: that board belongs to the parent turn's task line, and it must not write it.
                None,
                None,
            ) + "\n\nYou are a subagent: do the one task below and answer with the \
                 conclusion only. Whoever asked cannot see your steps, and you cannot ask \
                 follow-up questions.";

            // The tool table is the **parent turn's copy**, just without delegate_task — so recursive
            // delegation is not "denied", it is an option that does not exist at all.
            // The subagent's table has neither `delegate_task` (recursion structurally absent)
            // nor `todo_write`: the board belongs to **the parent turn's task line**; the subagent
            // writing it would overwrite the parent turn's plan.
            let child_tools = tools
                .without(joyczl_tools::subagent::NAME)
                .without("todo_write");
            // The subagent has **no approval channel** (`None`): it must not block on a human, so
            // actions requiring approval are treated as rejected outright — see limitations.md.
            //
            // Give it a session name of its own: logs and spill attribution can then trace "who made this".
            let child_session = format!(
                "subagent:{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            );
            let ctx = crate::tool_ctx(
                &facts,
                &episodes,
                &chat,
                &calendar,
                &home,
                crate::Injections {
                    session_id: child_session.clone(),
                    // The subagent works in the same directory as the parent task — the parent turn's
                    // cwd is carried in via run()'s parent_cwd.
                    cwd,
                    // The subagent does not touch background jobs: that is the parent turn's business, and it has no approval channel.
                    jobs: None,
                    // The subagent has no approval channel (it must not block on a human), but it has hooks: tool-level
                    // events flow as usual (under the subagent's own session name).
                    approval: None,
                    hooks: hooks.clone(),
                },
            );

            let budget = crate::tool_result_budget(&settings);

            // SubagentStart: delegated work must also be visible (audit and "who is touching my things").
            // Observe-only event — the hook's words will not block this delegation.
            crate::fire_hook(
                &hooks,
                joyczl_tools::hooks::HookEvent::SubagentStart,
                serde_json::json!({
                    "session_id": child_session,
                    "task": task_preview,
                    "max_iterations": iterations,
                }),
            )
            .await;

            let result = run_child(
                &resolved,
                &child_tools,
                ctx.clone(),
                Prompt {
                    system: system.clone(),
                    // Empty history: the point of delegation is to leave the context burden on the parent turn's side.
                    history: Vec::new(),
                    user_message: task,
                },
                iterations,
                budget,
            )
            .await
            .map_err(|e| anyhow::anyhow!("模型调用失败：{e}"))?;

            crate::fire_hook(
                &hooks,
                joyczl_tools::hooks::HookEvent::SubagentStop,
                serde_json::json!({
                    "session_id": child_session,
                    "iterations": result.iterations,
                    "interrupted": result.interrupted,
                }),
            )
            .await;

            let mut summary = result.reply.trim().to_string();

            // ---- Structured result: when the parent model gave a schema, require valid JSON that passes validation.
            // On failure **retry once with the error attached** (also carrying the previous round's messages,
            // so it can see what it wrote); if it still fails, fall back to prose and state why — one
            // off-format answer must not waste the whole task (the same rule as "fail-open").
            if let Some(schema) = &result_schema {
                match coerce_structured(&summary, schema) {
                    Ok(json) => summary = json,
                    Err(why) => {
                        let nudge = format!(
                            "上一次的结论不能用作结构化结果：{why}\n\
                             请**只**回复符合下面 schema 的 JSON（不要代码块、不要多余解释）：\n{schema}"
                        );
                        let retry = run_child(
                            &resolved,
                            &child_tools,
                            ctx,
                            Prompt {
                                system,
                                history: result.messages.clone(),
                                user_message: nudge,
                            },
                            iterations,
                            budget,
                        )
                        .await;
                        match retry {
                            Ok(second) => match coerce_structured(second.reply.trim(), schema) {
                                Ok(json) => {
                                    eprintln!("(joy) 子代理的结构化结果重试一次后通过");
                                    summary = json;
                                }
                                Err(again) => {
                                    eprintln!("(joy) 子代理的结构化结果两次都没过校验：{again}");
                                    summary.push_str(&format!(
                                        "\n（结构化结果两次都没通过校验：{again}；上面是它的原始文字）"
                                    ));
                                }
                            },
                            Err(e) => {
                                summary.push_str(&format!("\n（结构化重试没跑成：{e}）"));
                            }
                        }
                    }
                }
            }
            if summary.is_empty() {
                summary = "（子代理没有给出结论。）".to_string();
            }
            // Append "what it used" after the conclusion: the parent turn sees only one tool call, and these
            // details are its only clue for judging "can this conclusion be trusted" — **failures especially
            // must be flagged**, otherwise the parent turn will believe the subagent sailed through, when it
            // actually fabricated a conclusion on top of a failed call.
            let used: Vec<String> = result
                .tool_calls
                .iter()
                .map(|call| {
                    if call.ok() {
                        call.name.clone()
                    } else {
                        format!("{}（失败）", call.name)
                    }
                })
                .collect();
            if !used.is_empty() {
                summary.push_str(&format!(
                    "\n（子代理跑了 {} 轮，用了：{}）",
                    result.iterations,
                    used.join("、")
                ));
            }
            Ok(summary)
        })
    }
}
