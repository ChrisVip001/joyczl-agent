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

/// How many subagents one call may fan out.
///
/// Four, not "unlimited": the cost is one full model loop each, and a batch is meant for "four
/// independent lookups", not "run my backlog". Depth stays 1 for the same reason.
pub const MAX_BATCH: usize = 4;

/// Run one delegation. Implementation lives in app-server (it has Server, model, tools).
pub trait SubagentRunner: Send + Sync {
    /// Run one task, returning the **conclusion text**; on failure, a human reason.
    ///
    /// If `result_schema` is given, the conclusion must conform to it: validation and
    /// the "retry once" are the implementer's job (the tool layer knows no validators);
    /// if it still fails, return prose and say why — one bad format shouldn't waste it.
    ///
    /// `readonly` is passed explicitly rather than inferred: a batch of subagents runs at once in
    /// four independent loops, where the parent's "one write at a time" barrier cannot reach. The
    /// implementer must therefore hand the child a table with no exclusive tools at all. Single
    /// delegations pass `false` and keep every tool they always had.
    fn run(
        &self,
        task: String,
        max_iterations: Option<i32>,
        result_schema: Option<Value>,
        // The parent turn's working directory (the subagent works alongside the parent).
        parent_cwd: Option<std::path::PathBuf>,
        readonly: bool,
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
             描述要自足；它也**不能再派生子代理**。别用它来逃避一件事——它只是换了个上下文。\
             一次派多件时（`tasks`）它们**只能读**：并发写同一个目录会互相覆盖，所以改文件和\
             命令留给单件委派、或者你自己做。"
                .to_string(),
        input_schema: json!({
            "type": "object",
            // 要么一件事，要么一并发出去的一组（各自独立、顺序回填）。
            "anyOf": [{"required": ["task"]}, {"required": ["tasks"]}],
            "properties": {
                "task": {
                    "type": "string",
                    "description": "要让子代理做的事，写成自足的指令（它看不到当前对话）"
                },
                "tasks": {
                    "type": "array",
                    "maxItems": MAX_BATCH,
                    "items": {"type": "string"},
                    "description": "一次派出去的几件**互不依赖**的活（最多 4 件）：它们并发跑，\
                                   结论按你给的顺序合并回来；批量的子代理**只读**（不能写文件、\
                                   不能执行命令）。有先后依赖、或者要动磁盘的，就分开调。"
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
            }
            // No top-level `required`: "one task" and "a batch" are two valid shapes, and the
            // `anyOf` above is what says so. A `required: ["task"]` next to it would quietly
            // make `tasks` unusable.
        }),
        handler: Arc::new(move |_ctx: ToolCtx, args: Value| {
            let runner = runner.clone();
            Box::pin(async move {
                let max_iterations = args
                    .get("max_iterations")
                    .and_then(Value::as_i64)
                    .map(|value| value as i32);
                let result_schema = args.get("result_schema").cloned().filter(|v| !v.is_null());
                let cwd = _ctx.cwd.clone();

                // One task or a batch. `tasks` wins when both are given: a call that says "these
                // four" and also "this one" is ambiguous, and guessing silently is worse than
                // following the more specific field.
                let tasks: Vec<String> = match args.get("tasks") {
                    Some(Value::Array(items)) if !items.is_empty() => items
                        .iter()
                        .map(|item| {
                            item.as_str().map(str::to_string).ok_or_else(|| {
                                anyhow::anyhow!("tasks 里每一项都得是字符串（收到 {item}）")
                            })
                        })
                        .collect::<anyhow::Result<_>>()?,
                    Some(Value::Array(_)) => {
                        return Err(anyhow::anyhow!(
                            "tasks 是空数组 —— 要么给一件事（task），要么给几件互不依赖的活"
                        ))
                    }
                    _ => vec![require_str(&args, "task")?],
                };
                if tasks.len() > MAX_BATCH {
                    return Err(anyhow::anyhow!(
                        "一次最多派 {MAX_BATCH} 个子代理（收到 {} 件）—— 每件都是一整轮模型循环，\
                         拆成几次调用比一次派十个便宜",
                        tasks.len()
                    ));
                }

                if tasks.len() == 1 {
                    let task = tasks.into_iter().next().expect("刚刚确认有一个");
                    // Single delegation keeps every tool it always had: the parent turn is still
                    // serializing behind this call, so a write here is the same write as before.
                    return match runner
                        .run(task, max_iterations, result_schema, cwd, false)
                        .await
                    {
                        // Subagent output is **paraphrased**: escape + report header, so it never reads as system text.
                        Ok(summary) => Ok(crate::report::as_report(&summary)),
                        Err(why) => Ok(format!("Error: 子代理没跑成：{why}")),
                    };
                }

                // Fan out: each task gets its own subagent loop (its own context, no approval
                // channel), all in flight together. Results are **put back in the order asked**,
                // so the caller can map answer N to question N without counting.
                //
                // `JoinSet` (tokio, already a dependency) rather than a new combinator crate: the
                // futures here are owned ('static + Send — the runner clones what it needs before
                // awaiting), which is exactly what spawning requires.
                let mut set = tokio::task::JoinSet::new();
                for (index, task) in tasks.iter().enumerate() {
                    // `readonly = true`: several loops are in flight, so no single barrier can keep
                    // two of them from writing the same file. The tool table loses its write/exec
                    // tools instead of pretending the ordering still holds.
                    let fut = runner.run(
                        task.clone(),
                        max_iterations,
                        result_schema.clone(),
                        cwd.clone(),
                        true,
                    );
                    set.spawn(async move { (index, fut.await) });
                }
                let mut slots: Vec<Option<String>> = vec![None; tasks.len()];
                while let Some(joined) = set.join_next().await {
                    match joined {
                        Ok((index, Ok(summary))) => {
                            slots[index] = Some(format!(
                                "### 子任务 {}：{}\n{summary}",
                                index + 1,
                                tasks[index]
                            ));
                        }
                        // One failure does not sink the batch: the other conclusions are still
                        // worth delivering, and the failure is stated where it happened.
                        Ok((index, Err(why))) => {
                            slots[index] = Some(format!(
                                "### 子任务 {}：{}\n（没跑成：{why}）",
                                index + 1,
                                tasks[index]
                            ));
                        }
                        Err(join_error) => eprintln!("(joy) 一个子代理崩了：{join_error}"),
                    }
                }
                let merged = slots
                    .into_iter()
                    .map(|slot| slot.unwrap_or_else(|| "（这个子任务没有结果）".to_string()))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                Ok(crate::report::as_report(&merged))
            })
        }),
    }
}
