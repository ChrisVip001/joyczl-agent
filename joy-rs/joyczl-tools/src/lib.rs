//! `joyczl-tools` — the tool registry.
//!
//! A tool is three things: a model-readable name + description, a JSON schema for
//! arguments, and a Rust function that actually runs it. That's all.
//!
//! **On failure, return the error as text to the model instead of crashing the loop.**
//! Seeing the error, the model can take another path or explain to the user — far
//! better than failing the whole turn.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use joyczl_provider::ToolSchema;
use joyczl_state::{Calendar, Chat, Episodes, Facts};
use serde_json::Value;

pub mod approval;
pub mod calendar;
pub mod declarative;
pub mod exec;
pub mod fs;
pub mod fs_manage;
pub mod fs_write;
pub mod handlers;
pub mod hooks;
pub mod jobs;
pub mod memory_admin;
pub mod messages;
pub mod notify;
mod notify_queue;
pub mod patch;
pub mod report;
pub mod resources;
pub mod roots;
pub mod schedules;
pub mod search;
pub mod spill;
pub mod subagent;
pub mod todo;
pub mod web;

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tools_tests;

#[cfg(test)]
#[path = "exec_tests.rs"]
mod exec_tests;

#[cfg(test)]
#[path = "roots_tests.rs"]
mod roots_tests;

#[cfg(test)]
#[path = "subagent_tests.rs"]
mod subagent_tests;

#[cfg(test)]
#[path = "approval_tests.rs"]
mod approval_tests;

#[cfg(test)]
#[path = "spill_tests.rs"]
mod spill_tests;

#[cfg(test)]
#[path = "report_tests.rs"]
mod report_tests;

#[cfg(test)]
#[path = "hooks_tests.rs"]
mod hooks_tests;

#[cfg(test)]
#[path = "notify_tests.rs"]
mod notify_tests;

#[cfg(test)]
#[path = "todo_tests.rs"]
mod todo_tests;

#[cfg(test)]
#[path = "fs_tests.rs"]
mod fs_tests;

#[cfg(test)]
#[path = "fs_write_tests.rs"]
mod fs_write_tests;

#[cfg(test)]
#[path = "patch_tests.rs"]
mod patch_tests;

#[cfg(test)]
#[path = "web_tests.rs"]
mod web_tests;

#[cfg(test)]
#[path = "declarative_tests.rs"]
mod declarative_tests;

#[cfg(test)]
#[path = "search_tests.rs"]
mod search_tests;

/// Host notification that a tool cannot establish its external outcome.
pub type UncertainSink = Arc<dyn Fn(&str) + Send + Sync>;

/// The host environment available to tool handlers.
#[derive(Clone)]
pub struct ToolCtx {
    /// Host-only signal that an external operation may have committed without a receipt.
    pub on_uncertain: Option<UncertainSink>,
    /// Host-acquired mutation permit, shared with any background child.
    pub resource_lease: Option<resources::Lease>,
    /// Execution identity assigned by the host; never supplied by tool arguments.
    pub task_run_id: Option<String>,
    pub facts: Facts,
    pub episodes: Episodes,
    pub chat: Chat,
    pub calendar: Calendar,
    pub home: PathBuf,
    /// This turn's working directory (session-level working-directory). None = process cwd.
    /// exec's fallback chain: args.cwd > cwd > process cwd; read_file/list_dir resolve against it too.
    pub cwd: Option<PathBuf>,
    /// Which session this turn belongs to. Logs and spills attribute by it (a subagent gets
    /// its own name, so "who produced this oversized result" stays traceable).
    pub session_id: String,
    /// Is there someone to ask for approval? `None` = no (subagent, `JOY_APPROVAL=never`,
    /// or a caller with no interactive surface) — actions needing approval are denied
    /// outright, per `approval.rs`'s "deny by default".
    pub approval: Option<Arc<dyn crate::approval::ApprovalBroker>>,
    /// Background job host. `None` = off (it toggles with `JOY_EXEC`).
    pub jobs: Option<Arc<dyn crate::jobs::JobRegistry>>,
    /// Lifecycle hooks. `None` = none configured (not a single subprocess spawned).
    ///
    /// Tool **input rewriting and blocking** are done by `ToolRegistry::execute` with its own
    /// copy of the hooks (it is the single choke point); a copy is carried here so handlers
    /// can fire events too — currently only `run_command`'s approval path (`PermissionRequest`).
    pub hooks: Option<Arc<crate::hooks::Hooks>>,
    /// This turn's web-search budget (`web.rs::TurnLimits`). Created once per turn
    /// and shared by every clone of the ctx, so the counter survives across the
    /// turn's tool calls; a fresh turn gets a fresh budget.
    pub turn_limits: Arc<crate::web::TurnLimits>,
    /// How `find_tools` widens this session's tool table (`JOY_TOOL_POLICY=deferred`).
    /// `None` = the table is complete already, and the tool says so.
    pub activate: Option<Activator>,
    /// The vector leg of hybrid retrieval. `None` = keywords only (`JOY_EMBEDDINGS` off, or
    /// the embedder was not configured) — search then degrades instead of failing.
    pub embedder: Option<Arc<joyczl_provider::embed::Embedder>>,
    /// Live output while a tool is still running (`run_command` pipes its stdout here).
    ///
    /// `None` = nobody is watching (CLI, eval, a background job) — the tool just collects and
    /// returns. The final result is **always** the tool's return value; deltas are a progress
    /// feed for a human, never the answer, so a dropped chunk costs nothing but a repaint.
    pub on_delta: Option<DeltaSink>,
}

/// One chunk of live tool output. `Fn` and not a stream: the consumer is a notification sink
/// (SSE) that must not block the tool, and backpressure on a human's screen is meaningless.
pub type DeltaSink = Arc<dyn Fn(&str) + Send + Sync>;

pub type BoxFut = Pin<Box<dyn std::future::Future<Output = Result<String>> + Send>>;
pub type Handler = Arc<dyn Fn(ToolCtx, Value) -> BoxFut + Send + Sync>;

#[derive(Clone)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub handler: Handler,
}

/// The schema shape must match what the provider expects; convert once, here.
impl Tool {
    pub fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
        }
    }
}

/// Tools that **never** hide behind `find_tools`.
///
/// Two reasons to be on this list. The gated ones (`run_command`, `write_file`, `edit`,
/// `delegate_task`) are capabilities a human switched on — burying them behind a discovery
/// step would make the switch look broken, and hiding them behind a *different* search than
/// the switch is how a permission model rots. The rest are the handful every turn plausibly
/// needs; deferring them costs more in discovery turns than it saves in schema tokens.
pub const ALWAYS_ON: &[&str] = &[
    "run_command",
    "write_file",
    "edit",
    "delegate_task",
    "todo_write",
    "find_tools",
    "current_time",
    "save_note",
    "search_memory",
    "read_memory_page",
    "read_skill",
    "read_file",
    "list_dir",
    "glob",
    "grep",
    // Scheduling is something people ask for in a sentence ("send me a briefing every morning at eight"). If the
    // model has to discover these first, the answer to that sentence is "let me go find the tool first" —
    // and before they existed, it was "pick a working directory", which is worse: a file outside every
    // workspace that `write_file` is right to refuse.
    "schedule_job",
    "list_schedules",
    "cancel_schedule",
];

/// One session's deferred-tool activation set (`JOY_TOOL_POLICY=deferred`).
///
/// A named type because the shape is genuinely nested (session map → set) and the nesting
/// says nothing at the use site: `Arc<Mutex<BTreeSet<String>>>` four times in a signature
/// reads as punctuation, not meaning.
pub type ActivationSet = Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>;

/// Activates deferred tools for one session, by search query. The registry hands it out;
/// `find_tools` calls it. Returns the names it opened (empty = nothing matched).
pub type Activator = Arc<dyn Fn(&str) -> Vec<String> + Send + Sync>;

#[derive(Clone, Default)]
pub struct ToolRegistry {
    resources: Option<Arc<dyn resources::ResourceCoordinator>>,
    /// Lifecycle hooks (see `hooks.rs`). Loaded on the app-server side (it knows home).
    hooks: Option<Arc<crate::hooks::Hooks>>,
    tools: BTreeMap<String, Tool>,
    /// Each tool's **precompiled** argument validator. Compiled once at registration, zero parsing at call time.
    ///
    /// The outer `Option` is "is this tool in the table", the inner is "does its schema compile":
    /// an MCP server may report a broken schema; then skip validation and run anyway —
    /// a broken remote schema must not make a tool unusable (but say so in the startup log;
    /// silently skipping validation makes people think arguments were checked).
    validators: BTreeMap<String, Option<jsonschema::Validator>>,
    /// `JOY_TOOL_POLICY=deferred`: names whose schema stays out of the request until this
    /// session activates them. Empty = eager, every schema rides (the default).
    ///
    /// Hiding a schema is a **token** decision, never a permission one: an activated tool is
    /// still subject to its switch and its approval gate, and a deferred tool is still
    /// callable by name — the table is what the model is shown, not what it may do.
    deferred: std::collections::BTreeSet<String>,
    /// Activated names. Per session, hence a shared cell rather than a set on the registry:
    /// the loop holds `&ToolRegistry` while `find_tools` must mutate this through `ToolCtx`.
    activated: Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
    /// Tools that must run **alone**. The loop turns each into a barrier: calls before it
    /// finish first, calls after it wait. Writes, exec, subagents and approval-carrying tools
    /// land here, so two writes never interleave and at most one approval card is up.
    exclusive: std::collections::BTreeSet<String>,
}

impl ToolRegistry {
    /// Install lifecycle hooks (app-server calls once after `builtin_tools`).
    /// Share one mutation coordinator across session copies of this registry.
    pub fn set_resources(&mut self, coordinator: Arc<dyn resources::ResourceCoordinator>) {
        self.resources = Some(coordinator);
    }

    pub fn set_hooks(&mut self, hooks: Option<Arc<crate::hooks::Hooks>>) {
        self.hooks = hooks;
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Remove one tool's copy (for subagents: they cannot delegate further).
    ///
    /// Clone the whole registry instead of sharing — the subagent's table and the parent's
    /// then never affect each other, and tools are `Arc`-wrapped, so cloning is cheap.
    pub fn without(&self, name: &str) -> Self {
        let mut copy = self.clone();
        copy.tools.remove(name);
        copy.validators.remove(name);
        copy
    }

    /// A copy with every **exclusive** tool removed — the table a batched subagent gets.
    ///
    /// Why remove instead of serialize: four subagents run at once, each in its own loop, so the
    /// parent's barrier cannot reach them. Two of them writing the same directory means the last
    /// writer wins — silently. Taking the write/exec tools away makes "batch = read-only" a fact
    /// of the tool table rather than a rule someone has to remember.
    ///
    /// Both maps are trimmed together: a validator without a tool (or the reverse) is a table
    /// that half-exists, which is exactly the kind of inconsistency nobody debugs later.
    pub fn readonly(&self) -> Self {
        let mut copy = self.clone();
        for name in &self.exclusive {
            copy.tools.remove(name);
            copy.validators.remove(name);
        }
        // The scheduling flags go with the tools: a barrier for something that cannot be called is
        // a leftover that later reads as "this table still has an exclusive tool".
        copy.exclusive.clear();
        copy.deferred.retain(|name| copy.tools.contains_key(name));
        copy
    }

    pub fn register(&mut self, tool: Tool) {
        // Same name **never overwrites**: the later one is rejected.
        //
        // A name has exactly one implementation — built-ins, MCP servers, and skills can all
        // collide, and "quietly swapping a tool's implementation" is the worst possible accident
        // (an MCP tool named run_command must never displace the real run_command). Better one
        // tool fewer than one tool whose executor is unknown.
        if self.tools.contains_key(&tool.name) {
            eprintln!(
                "(joy) the tool name '{}' is already taken; the later registration is refused (it does not overwrite the existing one)",
                tool.name
            );
            return;
        }
        let validator = match jsonschema::validator_for(&tool.input_schema) {
            Ok(validator) => Some(validator),
            Err(e) => {
                eprintln!(
                    "(joy) tool '{}' has an input_schema that does not compile ({e}) — its arguments are not validated",
                    tool.name
                );
                None
            }
        };
        self.validators.insert(tool.name.clone(), validator);
        self.tools.insert(tool.name.clone(), tool);
    }

    /// Register a tool that must run **alone** (write / exec / subagent / anything that asks
    /// a human). A read-only tool registered normally may share a batch with other read-only
    /// calls; this one is always a barrier.
    ///
    /// The flag lives on the registry rather than on `Tool` because it is a scheduling
    /// property of this table, not of the implementation: the same MCP tool may be one or the
    /// other depending on where it was mounted.
    pub fn register_exclusive(&mut self, tool: Tool) {
        self.exclusive.insert(tool.name.clone());
        self.register(tool);
    }

    /// Must this call run alone? Unknown names are not exclusive — they fail fast as text,
    /// and a made-up name must not change scheduling.
    pub fn is_exclusive(&self, name: &str) -> bool {
        self.exclusive.contains(name)
    }

    pub fn get(&self, name: &str) -> Option<&Tool> {
        self.tools.get(name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    /// Defer every tool not on `ALWAYS_ON` (`JOY_TOOL_POLICY=deferred`).
    ///
    /// Called once, after the table is assembled: the policy is a shape of the request, not
    /// a property of each registration.
    pub fn defer_all_but_always(&mut self) {
        for name in self.tools.keys() {
            if !ALWAYS_ON.contains(&name.as_str()) {
                self.deferred.insert(name.clone());
            }
        }
    }

    /// A per-session copy: same tools, fresh activation set.
    ///
    /// Per session on purpose — "I showed the model the calendar schema" is a fact about
    /// this conversation, and one session's discovery must not quietly widen another's request.
    pub fn session_view(&self) -> Self {
        self.session_view_with(Arc::new(std::sync::Mutex::new(Default::default())))
    }

    /// A view over an **existing** activation set.
    ///
    /// `find_tools` is called in one turn and its result lands in the next, so the set has
    /// to outlive a single turn. The caller (app-server) owns the per-session cell; a fresh
    /// registry on every turn would make the model rediscover the same tool every message —
    /// strictly worse than the eager table it replaced.
    pub fn session_view_with(&self, activated: ActivationSet) -> Self {
        let mut view = self.clone();
        view.activated = activated;
        view
    }

    /// Hand `find_tools` the ability to activate, without giving it the whole registry.
    ///
    /// Matching is deliberately plain: lowercase substring of the query in the name or the
    /// description. A semantic search over twelve tool descriptions would need a model call
    /// to save a few hundred tokens — the trade is backwards, and "why did this not match"
    /// must stay answerable by reading the code.
    pub fn activator(&self) -> Activator {
        let catalogue: Vec<(String, String)> = self
            .tools
            .values()
            .map(|tool| (tool.name.clone(), tool.description.to_lowercase()))
            .collect();
        let deferred = self.deferred.clone();
        let activated = self.activated.clone();
        Arc::new(move |query: &str| {
            let Ok(mut set) = activated.lock() else {
                return Vec::new();
            };
            // Hidden tools first — an already-visible one needs no activation, and saying so
            // would spend a round teaching the model nothing.
            let hidden: Vec<&(String, String)> = catalogue
                .iter()
                .filter(|(name, _)| deferred.contains(name) && !set.contains(name))
                .collect();
            let needle = query.to_lowercase();
            let words: Vec<&str> = needle.split_whitespace().collect();
            let mut hit: Vec<String> = hidden
                .iter()
                .filter(|(name, desc)| {
                    words
                        .iter()
                        .any(|word| name.to_lowercase().contains(word) || desc.contains(word))
                })
                .map(|(name, _)| name.clone())
                .collect();
            if hit.is_empty() {
                // No keyword landed: hand over the whole hidden shelf rather than a dead end.
                // The model asked because it needs *something*, and one round of reading
                // twelve names beats three rounds of guessing words.
                hit = hidden.iter().map(|(name, _)| name.clone()).collect();
            }
            for name in &hit {
                set.insert(name.clone());
            }
            hit
        })
    }

    /// What the model sees: everything in eager mode, `ALWAYS_ON` + activated in deferred.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        let activated = self
            .activated
            .lock()
            .map(|set| set.clone())
            .unwrap_or_default();
        self.tools
            .values()
            .filter(|tool| !self.deferred.contains(&tool.name) || activated.contains(&tool.name))
            .map(Tool::schema)
            .collect()
    }

    /// Run one call. **Never returns Err** — errors become text the model can read.
    /// Unknown tools too; the model can correct itself.
    pub async fn execute(&self, mut ctx: ToolCtx, name: &str, args: Value) -> String {
        let Some(tool) = self.tools.get(name) else {
            return format!(
                "Error: no tool named '{name}'. Available: {}",
                self.names().join(", ")
            );
        };

        // Validate args against the schema before handing to the handler. The order matters:
        // errors can name the field and the expectation (the model fixes it right), handlers
        // needn't re-check types; and on validation failure the handler never runs — no half side effects.
        if let Some(Some(validator)) = self.validators.get(name) {
            if let Err(why) = describe_violations(validator, &args) {
                return format!(
                    "Error: the arguments do not match the {name} schema — {why}. Rewrite the input to satisfy the schema."
                );
            }
        }

        // Delegation orchestrates child tools, which acquire their own permits.
        // Holding one here would deadlock a delegated mutation.
        if self.is_exclusive(name) && name != "delegate_task" {
            if let Some(resources) = &self.resources {
                match resources.acquire(name).await {
                    Ok(lease) => ctx.resource_lease = Some(lease),
                    Err(error) => return format!("Error: {error}"),
                }
            }
        }

        // ---- PreToolUse: can block, or rewrite the input args.
        let mut args = args;
        if let Some(hooks) = &self.hooks {
            let payload = serde_json::json!({
                "session_id": ctx.session_id,
                "cwd": std::env::current_dir().ok(),
                "tool_name": name,
                "tool_input": args,
            });
            let outcome = hooks
                .fire(crate::hooks::HookEvent::PreToolUse, payload)
                .await;
            if let Some(reason) = outcome.blocked {
                // Handed over as `Error:`: to the model it's a rejected call, and it should adapt.
                return format!("Error: {reason} (a PreToolUse hook refused to run {name})");
            }
            if let Some(rewritten) = outcome.input {
                // **Re-validate after rewriting**: hooks can break things too, and "who broke it"
                // must be clear, or the model will think it wrote the args wrong.
                if let Some(Some(validator)) = self.validators.get(name) {
                    if let Err(why) = describe_violations(validator, &rewritten) {
                        return format!(
                            "Error: the arguments rewritten by a PreToolUse hook do not match the {name} schema — {why}"
                        );
                    }
                }
                args = rewritten;
            }
        }

        let output = match (tool.handler)(ctx.clone(), args).await {
            Ok(output) => output,
            Err(e) => format!("Error: {name} failed: {e}"),
        };

        // ---- PostToolUse / PostToolUseFailure: may rewrite the result; a block replaces it
        // with the reason ("denied" is more honest to the model than a doctored success).
        let Some(hooks) = &self.hooks else {
            return output;
        };
        let failed = output.starts_with("Error:");
        let event = if failed {
            crate::hooks::HookEvent::PostToolUseFailure
        } else {
            crate::hooks::HookEvent::PostToolUse
        };
        let payload = serde_json::json!({
            "session_id": ctx.session_id,
            "cwd": std::env::current_dir().ok(),
            "tool_name": name,
            "tool_output": output,
        });
        let outcome = hooks.fire(event, payload).await;
        if let Some(reason) = outcome.blocked {
            return format!(
                "Error: {reason} (a {} hook refused the result of {name})",
                event.name()
            );
        }
        outcome.output.unwrap_or(output)
    }
}

/// Validate a JSON value against a schema; the error is the model-facing sentence (`Ok(())` if valid).
///
/// Tool-argument validation goes through the same path (`describe_violations`), and so does
/// the subagent's structured result — the wording is identical, so the model learns one dialect.
pub fn validate_value(schema: &Value, value: &Value) -> Result<(), String> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|e| format!("this schema does not compile: {e}"))?;
    describe_violations(&validator, value)
}

/// Translate schema violations into one model-facing sentence: at most three, each with a field path.
///
/// No dumping the whole schema — it would flood the context, and the model only needs "what's wrong".
/// Returns `Ok(())` when everything is valid.
fn describe_violations(validator: &jsonschema::Validator, args: &Value) -> Result<(), String> {
    let all: Vec<String> = validator
        .iter_errors(args)
        .map(|error| {
            let path = error.instance_path();
            if path.is_empty() {
                format!("the arguments themselves: {error}")
            } else {
                format!("{path}：{error}")
            }
        })
        .collect();
    if all.is_empty() {
        return Ok(());
    }
    let shown = all.len().min(3);
    let mut text = all[..shown].join("；");
    if all.len() > shown {
        text.push_str(&format!("({} more)", all.len() - shown));
    }
    Err(text)
}

// ---- Small helpers reused inside handlers ---------------------------------

/// Take a required string argument. Clear errors for missing/wrong type — this text lands
/// in the model's context; reading it, it knows how to fix things.
pub fn require_str(args: &Value, key: &str) -> Result<String> {
    let value = args
        .get(key)
        .ok_or_else(|| anyhow::anyhow!("Missing parameter '{key}'"))?;
    let text = value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Parameter '{key}' should be a string, got {value}"))?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Parameter '{key}' must not be empty");
    }
    Ok(trimmed.to_string())
}

/// Take an optional integer argument (with a default).
pub fn opt_u32(args: &Value, key: &str, default: u32) -> Result<u32> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v.as_u64().map(|n| n as u32).ok_or_else(|| {
            anyhow::anyhow!("Parameter '{key}' should be a non-negative integer, got {v}")
        }),
    }
}
