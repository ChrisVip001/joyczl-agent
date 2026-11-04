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
pub mod exec;
pub mod fs;
pub mod fs_write;
pub mod handlers;
pub mod hooks;
pub mod jobs;
pub mod memory_admin;
pub mod messages;
pub mod notify;
pub mod report;
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
#[path = "web_tests.rs"]
mod web_tests;

/// What a tool's execution can see. Adding a field takes thought: every tool sees all of it.
/// Deliberately Clone — the handler's Future must own it, so it can be 'static.
#[derive(Clone)]
pub struct ToolCtx {
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
}

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

#[derive(Clone, Default)]
pub struct ToolRegistry {
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
}

impl ToolRegistry {
    /// Install lifecycle hooks (app-server calls once after `builtin_tools`).
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

    pub fn register(&mut self, tool: Tool) {
        // Same name **never overwrites**: the later one is rejected.
        //
        // A name has exactly one implementation — built-ins, MCP servers, and skills can all
        // collide, and "quietly swapping a tool's implementation" is the worst possible accident
        // (an MCP tool named run_command must never displace the real run_command). Better one
        // tool fewer than one tool whose executor is unknown.
        if self.tools.contains_key(&tool.name) {
            eprintln!(
                "(joy) 工具名 '{}' 已经被占用了，后到的这个被拒绝注册（不覆盖已有的）",
                tool.name
            );
            return;
        }
        let validator = match jsonschema::validator_for(&tool.input_schema) {
            Ok(validator) => Some(validator),
            Err(e) => {
                eprintln!(
                    "(joy) 工具 '{}' 的 input_schema 编译不了（{e}）—— 这个工具的参数不做校验",
                    tool.name
                );
                None
            }
        };
        self.validators.insert(tool.name.clone(), validator);
        self.tools.insert(tool.name.clone(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&Tool> {
        self.tools.get(name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.values().map(Tool::schema).collect()
    }

    /// Run one call. **Never returns Err** — errors become text the model can read.
    /// Unknown tools too; the model can correct itself.
    pub async fn execute(&self, ctx: ToolCtx, name: &str, args: Value) -> String {
        let Some(tool) = self.tools.get(name) else {
            return format!(
                "Error: 没有叫 '{name}' 的工具。可用：{}",
                self.names().join(", ")
            );
        };

        // Validate args against the schema before handing to the handler. The order matters:
        // errors can name the field and the expectation (the model fixes it right), handlers
        // needn't re-check types; and on validation failure the handler never runs — no half side effects.
        if let Some(Some(validator)) = self.validators.get(name) {
            if let Err(why) = describe_violations(validator, &args) {
                return format!(
                    "Error: 参数不符合 {name} 的 schema —— {why}。请重写输入以满足 schema。"
                );
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
                return format!("Error: {reason}（PreToolUse hook 拒绝执行 {name}）");
            }
            if let Some(rewritten) = outcome.input {
                // **Re-validate after rewriting**: hooks can break things too, and "who broke it"
                // must be clear, or the model will think it wrote the args wrong.
                if let Some(Some(validator)) = self.validators.get(name) {
                    if let Err(why) = describe_violations(validator, &rewritten) {
                        return format!(
                            "Error: PreToolUse hook 改写后的参数不符合 {name} 的 schema —— {why}"
                        );
                    }
                }
                args = rewritten;
            }
        }

        let output = match (tool.handler)(ctx.clone(), args).await {
            Ok(output) => output,
            Err(e) => format!("Error: 执行 {name} 失败：{e}"),
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
                "Error: {reason}（{} hook 拒绝了 {name} 的结果）",
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
    let validator =
        jsonschema::validator_for(schema).map_err(|e| format!("这份 schema 本身编译不了：{e}"))?;
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
                format!("参数本身：{error}")
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
        text.push_str(&format!("（还有 {} 处）", all.len() - shown));
    }
    Err(text)
}

// ---- Small helpers reused inside handlers ---------------------------------

/// Take a required string argument. Clear errors for missing/wrong type — this text lands
/// in the model's context; reading it, it knows how to fix things.
pub fn require_str(args: &Value, key: &str) -> Result<String> {
    let value = args
        .get(key)
        .ok_or_else(|| anyhow::anyhow!("缺少参数 '{key}'"))?;
    let text = value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("参数 '{key}' 应该是字符串，得到的是 {value}"))?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        anyhow::bail!("参数 '{key}' 不能是空的");
    }
    Ok(trimmed.to_string())
}

/// Take an optional integer argument (with a default).
pub fn opt_u32(args: &Value, key: &str, default: u32) -> Result<u32> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_u64()
            .map(|n| n as u32)
            .ok_or_else(|| anyhow::anyhow!("参数 '{key}' 应该是非负整数，得到的是 {v}")),
    }
}
