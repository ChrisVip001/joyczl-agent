//! 子代理：把一件独立的活交出去跑，只把结论带回来。
//!
//! 这一层**不认识** Server、loop、provider —— 它只有一个 trait，实现由
//! `joyczl-app-server` 注入（依赖方向不能反过来：loop 依赖 tools）。
//!
//! 三条设计边界：
//!
//! 1. **只回摘要**。子代理的完整消息列表不回传 —— 那会把父的上下文撑爆，
//!    而「把上下文交出去又整个拿回来」等于没省。
//! 2. **不能再派生**。实现方构造子代理的工具表时会显式去掉 `delegate_task`
//!    （codex 的 disables-collab 做法）。递归派生是那种「平时看不出来、
//!    一旦有人试就烧穿配额」的东西。
//! 3. **不参与交互批准**。子代理不该阻塞在人类输入上：它拿不到批准通道，
//!    需要批准的动作直接按「拒绝」处理（见 `docs/limitations.md`）。

use std::sync::Arc;

use serde_json::{json, Value};

use crate::{require_str, BoxFut, Tool, ToolCtx};

/// 工具名。实现方要把它从子代理的工具表里去掉，所以名字得是公开的常量 ——
/// 让两边拼字符串迟早会拼歪。
pub const NAME: &str = "delegate_task";

/// 执行一次委派。实现放在 app-server（它才有 Server、模型与工具表）。
pub trait SubagentRunner: Send + Sync {
    /// 跑一件活，返回**结论文本**。失败返回一句给人看的原因。
    fn run(&self, task: String, max_iterations: Option<i32>) -> BoxFut;
}

/// 注册进工具表的 `delegate_task`。只有 `JOY_DELEGATE=1` 时才会被注册 ——
/// 没开的时候模型连它的名字都看不见（与 `run_command` 同一条规矩）。
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
                match runner.run(task, max_iterations).await {
                    Ok(summary) => Ok(format!("子代理回话了：\n{summary}")),
                    Err(why) => Ok(format!("Error: 子代理没跑成：{why}")),
                }
            })
        }),
    }
}
