//! Subagent: hand off a self-contained piece of work and bring back only the conclusion.
//!
//! This layer **knows nothing about** Server, loop, or provider — it has only a trait,
//! injected by `joyczl-app-server` (dependency direction must not reverse: loop → tools).
//!
//! Three design boundaries:
//!
//! 1. **Summary only**. The subagent's full message list is not returned — that would
//!    blow up the parent's context; handing it out and getting it all back saves nothing.
//! 2. **No further delegation**. The implementer explicitly removes `delegate_task` when
//!    building the subagent's tool table (recursive delegation is forbidden) — the kind
//!    of thing that "looks harmless until someone tries it and burns through quota".
//! 3. **No interactive approval**. A subagent must not block on human input: it gets no
//!    approval channel, and actions needing approval are treated as "denied" outright.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::{require_str, BoxFut, Tool, ToolCtx};

/// Tool name. The implementer must remove it from the subagent's tool table, so it has
/// to be a public constant — letting both sides assemble the string invites drift.
pub const NAME: &str = "delegate_task";

/// Run one delegation. Implementation lives in app-server (it has Server, model, tools).
pub trait SubagentRunner: Send + Sync {
    /// Run one task, returning the **conclusion text**; on failure, a human reason.
    ///
    /// If `result_schema` is given, the conclusion must conform to it: validation and
    /// the "retry once" are the implementer's job (the tool layer knows no validators);
    /// if it still fails, return prose and say why — one bad format shouldn't waste it.
    fn run(
        &self,
        task: String,
        max_iterations: Option<i32>,
        result_schema: Option<Value>,
        // The parent turn's working directory (the subagent works alongside the parent).
        parent_cwd: Option<std::path::PathBuf>,
    ) -> BoxFut;
}

/// The `delegate_task` registered into the tool table. Only registered when `JOY_DELEGATE=1`
/// — when off, the model cannot even see its name (same rule as `run_command`).
pub fn delegate_task(runner: Arc<dyn SubagentRunner>) -> Tool {
    Tool {
        name: NAME.to_string(),
        description:
            "把一件**独立**的活交给子代理去跑，只拿结论回来。适合「查清楚 X」「把 Y 整理成清单」\
             这类自成一体、不需要和用户来回确认的任务。子代理看不到你们刚才的对话，所以任务\
             描述要自足；它也**不能再派生子代理**。别用它来逃避一件事——它只是换了个上下文。"
                .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "要让子代理做的事，写成自足的指令（它看不到当前对话）"
                },
                "max_iterations": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 10,
                    "description": "子代理最多跑几轮（默认 5，上限 10）"
                },
                "result_schema": {
                    "type": "object",
                    "description": "想要结构化结论就给一份 JSON Schema：子代理必须回符合它的 \
                                   JSON。不合规会带着报错重试一次，仍不合规就回落成散文并在 \
                                   文本里说明 —— 结论的形状由你定，不靠它自觉。"
                }
            },
            "required": ["task"]
        }),
        handler: Arc::new(move |_ctx: ToolCtx, args: Value| {
            let runner = runner.clone();
            Box::pin(async move {
                let task = require_str(&args, "task")?;
                let max_iterations = args
                    .get("max_iterations")
                    .and_then(Value::as_i64)
                    .map(|value| value as i32);
                let result_schema = args.get("result_schema").cloned().filter(|v| !v.is_null());
                match runner
                    .run(task, max_iterations, result_schema, _ctx.cwd.clone())
                    .await
                {
                    // Subagent output is **paraphrased**: escape + report header, so it never reads as system text.
                    Ok(summary) => Ok(crate::report::as_report(&summary)),
                    Err(why) => Ok(format!("Error: 子代理没跑成：{why}")),
                }
            })
        }),
    }
}
