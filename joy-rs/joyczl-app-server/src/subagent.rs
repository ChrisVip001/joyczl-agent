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
/// A subagent is **hard-capped** on cost, independently of `JOY_TURN_TOKEN_BUDGET`: ten rounds of
/// a big context would otherwise be someone's whole bill for a side question. Same exit as the
/// parent's budget — tools away, one wrap-up answer.
const SUBAGENT_TOKEN_BUDGET: Option<usize> = Some(120_000);

/// Distinguishes subagents spawned within the same millisecond (a batch spawns up to four).
static SUBAGENT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
    /// The stable half, passed through to the loop untouched so the cache boundary holds.
    system_prefix: Option<String>,
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
        system_prefix: prompt.system_prefix,
        client: resolved.client.as_ref(),
        model: &resolved.model,
        system: prompt.system,
        history: prompt.history,
        user_message: prompt.user_message,
        user_images: Vec::new(),
        tools,
        ctx,
        max_iterations,
        max_tokens: SUBAGENT_MAX_TOKENS,
        token_budget: SUBAGENT_TOKEN_BUDGET,
        tool_result_budget: budget,
        observer: None,
        on_text: None,
        interrupt: None,
        checkpoint: None,
        steering: None,
        resume: None,
    })
    .await
}

/// Squeeze the subagent's conclusion into a "schema-valid JSON string".
fn coerce_structured(reply: &str, schema: &Value) -> Result<String, String> {
    let Some(json) = joyczl_memory::gate::extract_json(reply) else {
        return Err("no JSON object found in the reply".to_string());
    };
    let Ok(value) = serde_json::from_str::<Value>(&json) else {
        return Err("that JSON does not parse".to_string());
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
        readonly: bool,
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
            let Some(resolved) = resolved
                .read()
                .expect("the resolved lock should not be poisoned")
                .clone()
            else {
                return Err(anyhow::anyhow!(
                    "the model is not configured; a subagent cannot start"
                ));
            };
            let settings = settings
                .read()
                .expect("the settings lock should not be poisoned")
                .clone();
            let home: PathBuf = settings.home.clone();

            let iterations = max_iterations
                .unwrap_or(DEFAULT_ITERATIONS)
                .clamp(1, MAX_ITERATIONS);
            // The event payload uses a short head of the task: it may be long, and the hook wants "what is being done".
            let task_preview: String = task.chars().take(400).collect();

            // The tool table is the **parent turn's copy**, just without delegate_task — so recursive
            // delegation is not "denied", it is an option that does not exist at all.
            // The subagent's table has neither `delegate_task` (recursion structurally absent)
            // nor `todo_write`: the board belongs to **the parent turn's task line**; the subagent
            // writing it would overwrite the parent turn's plan.
            let child_tools = tools
                .without(joyczl_tools::subagent::NAME)
                .without("todo_write");
            // A batch member runs **read-only**: four loops are in flight, so the parent's
            // "one write at a time" barrier cannot reach any of them. Dropping the exclusive
            // tools makes that a property of the table instead of a rule to remember.
            let child_tools = if readonly {
                child_tools.readonly()
            } else {
                child_tools
            };

            let soul = crate::turn::load_soul(
                &home,
                crate::turn::thin_soul_for(&settings.soul_mode, &resolved.provider_id),
            );
            let prompt = crate::turn::build_system(crate::turn::PromptParts {
                soul: &soul,
                model: &resolved.model,
                provider: &resolved.provider_id,
                memory: "",
                // No catalog and no memory: the subagent's contract is "the task in, the
                // conclusion out" — a shared library would blur who said what.
                memory_index: None,
                skill_index: None,
                skills: "",
                // The subagent has no todo list: that board belongs to the parent turn's task line, and it must not write it.
                todo: None,
                summary: None,
                // No project instructions either: the subagent gets one task in and one
                // conclusion out — repository-wide conventions would only blur its brief.
                instructions: None,
                // Same machine, same gaps: a subagent that does not know there is no shell
                // burns its budget the same way the parent did.
                capabilities: crate::turn::capability_note(
                    &settings,
                    !child_tools.schemas().is_empty(),
                )
                .as_deref(),
            });
            // Same stable prefix as the parent (soul + model identity), so the provider's
            // cache serves both; the subagent's own contract rides in the tail — it is not
            // the same instructions, and pretending otherwise would be a lie to the cache.
            let system_prefix = Some(prompt.stable);
            let system = prompt.tail
                + "\n\nYou are a subagent: do the one task below and answer with the \
                   conclusion only. Whoever asked cannot see your steps, and you cannot ask \
                   follow-up questions."
                // Say it before it tries: a child that discovers "no write tools" by calling one
                // has burned a round of its five, and its conclusion becomes "I could not".
                + if readonly {
                    "\n\nYou can only READ in this task: writing files and running commands are \
                     not available (several subagents are running at once). If the job needs a \
                     change, say so in the conclusion instead of attempting it."
                } else {
                    ""
                };

            // The subagent has **no approval channel** (`None`): it must not block on a human, so
            // actions requiring approval are treated as rejected outright — see limitations.md.
            //
            // Give it a session name of its own: logs and spill attribution can then trace "who made this".
            //
            // The counter is not decoration: a batch spawns up to four children in the same
            // millisecond, and a shared name makes their hook events and spill files
            // indistinguishable — which is exactly when you need to tell them apart.
            let child_session = format!(
                "subagent:{}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0),
                SUBAGENT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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
                    // A hook consumer can tell "this child could write" from "it could not".
                    "readonly": readonly,
                }),
            )
            .await;

            let result = run_child(
                &resolved,
                &child_tools,
                ctx.clone(),
                Prompt {
                    system_prefix: system_prefix.clone(),
                    system: system.clone(),
                    // Empty history: the point of delegation is to leave the context burden on the parent turn's side.
                    history: Vec::new(),
                    user_message: task,
                },
                iterations,
                budget,
            )
            .await
            .map_err(|e| anyhow::anyhow!("Model call failed: {e}"))?;

            crate::fire_hook(
                &hooks,
                joyczl_tools::hooks::HookEvent::SubagentStop,
                serde_json::json!({
                    "session_id": child_session,
                    "iterations": result.iterations,
                    "interrupted": result.interrupted,
                    "readonly": readonly,
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
                            "The previous answer cannot be used as a structured result: {why}\nReply with **only** JSON that matches the schema below (no code fence, no extra explanation):\n{schema}"
                        );
                        let retry = run_child(
                            &resolved,
                            &child_tools,
                            ctx,
                            Prompt {
                                system_prefix: system_prefix.clone(),
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
                                    eprintln!("(joy) the subagent's structured result passed on the retry");
                                    summary = json;
                                }
                                Err(again) => {
                                    eprintln!("(joy) the subagent's structured result failed validation twice: {again}");
                                    summary.push_str(&format!(
                                        "\n(the structured result failed validation twice: {again}; above is its raw text)"
                                    ));
                                }
                            },
                            Err(e) => {
                                summary.push_str(&format!(
                                    "\n(the structured retry did not run: {e})"
                                ));
                            }
                        }
                    }
                }
            }
            if summary.is_empty() {
                summary = "(the subagent reached no conclusion.)".to_string();
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
                        format!("{} (failed)", call.name)
                    }
                })
                .collect();
            if !used.is_empty() {
                summary.push_str(&format!(
                    "\n(the subagent ran {} rounds and used: {})",
                    result.iterations,
                    used.join("、")
                ));
            }
            Ok(summary)
        })
    }
}
