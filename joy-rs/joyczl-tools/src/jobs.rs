//! 后台作业：长命令不该占着整轮对话。
//!
//! 一条 `cargo test` 要跑三分钟。前台 `run_command` 会把这三分种**堵在回合里**——
//! 用户看着一个不动的界面，上下文里也没有任何别的东西产生。后台作业把这两件事
//! 分开：命令在一个独立进程里跑，输出进一个有界的环，模型想看了就来读一截。
//!
//! 三条规矩：
//!
//! * **授权靠 owner 围栏，不靠 id 保密**（抄 deepseek-harness）：job id 是可预测
//!   的短字符串，但只有**起它的那个会话**能读、能停。别的会话拿到 id 也没用。
//! * **输出是有界的环**：溢出丢最旧的字节，读的时候**如实说丢了多少**，而不是
//!   报错。一个跑飞的命令不该把内存吃光，也不该让「读一次」变成失败。
//! * **闸门与前台同一条**：后台命令照样过 vet 与批准（见 exec.rs 的 `pass_gate`）
//!   —— 「先放后台再慢慢跑」正是绕过批准最自然的方式。
//!
//! 生命周期：作业随 Joy 进程生死。进程没了，子进程也收（`kill_on_drop`），所以
//! 「重启后继续」这件事在这里没有意义 —— 它不是守卫进程，是这一趟的助手。

use serde_json::{json, Value};

use crate::Tool;

/// job 的 id（可预测的短字符串，见模块文档）。
pub type JobId = String;

/// 读一截输出。
#[derive(Debug, Clone)]
pub struct JobRead {
    /// 从 `cursor` 开始的新输出。
    pub text: String,
    /// 下次该传进来的位置。
    pub cursor: u64,
    pub running: bool,
    /// 环溢出过：`dropped` 说的那些字节已经读不到了。
    pub lossy: bool,
    pub dropped: u64,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct JobSummary {
    pub id: JobId,
    pub command: String,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub bytes: u64,
}

/// 后台作业的宿主能力。工具层只知道这个 trait，实现（进程 + 环 + 落盘）在
/// app-server —— 与 `SubagentRunner`、`ApprovalBroker` 同一条注入纪律。
pub type JobFut<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;

pub trait JobRegistry: Send + Sync {
    /// 起一个作业。`owner` 是会话名（授权围栏）；`cwd` 与前台 `run_command`
    /// 的 `cwd` 同义（沙箱的可写根从它算起）。
    fn spawn(
        &self,
        owner: &str,
        command: &str,
        cwd: Option<String>,
    ) -> JobFut<Result<JobId, String>>;
    /// 读一截。`wait` 为真时有界地等一会儿（等出结果或等到上限）。
    fn output(
        &self,
        owner: &str,
        id: &JobId,
        cursor: u64,
        wait: bool,
    ) -> JobFut<Result<JobRead, String>>;
    /// 停掉它。返回「本来还在跑吗」。
    fn kill(&self, owner: &str, id: &JobId) -> JobFut<Result<bool, String>>;
    /// 这个会话自己的作业（别人的不列）。
    fn list(&self, owner: &str) -> Vec<JobSummary>;
}

/// 没有作业宿主时给模型的答复（默认情况下后台能力是关的）。
fn no_registry() -> String {
    "Error: 这台机器上没有开启后台作业（它跟着 run_command 一起开关：JOY_EXEC=1）。".to_string()
}

/// `job_output`：读一截输出。读完要接着读就带上上次给的 `cursor`。
pub fn job_output(jobs: Option<std::sync::Arc<dyn JobRegistry>>) -> Tool {
    Tool {
        name: "job_output".to_string(),
        description: "读一个后台作业的输出。第一次不带 cursor（从头读），之后带上上次答复里\
                      给的 cursor 接着读。作业还没结束时 wait=true 会有界地等一会儿。"
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": { "type": "string", "description": "job id（run_command 的 background=true 会给）" },
                "cursor": { "type": "integer", "minimum": 0, "description": "从哪儿接着读（上次答复里给的）" },
                "wait": { "type": "boolean", "description": "还没结束就等一会儿（有上限，超了如实返回仍在跑）" }
            }
        }),
        handler: std::sync::Arc::new(move |ctx: crate::ToolCtx, args: Value| {
            let jobs = jobs.clone();
            Box::pin(async move {
                let Some(jobs) = jobs else {
                    return Ok(no_registry());
                };
                let Some(id) = args.get("id").and_then(Value::as_str) else {
                    return Ok("Error: 缺少 id".to_string());
                };
                let cursor = args.get("cursor").and_then(Value::as_u64).unwrap_or(0);
                let wait = args.get("wait").and_then(Value::as_bool).unwrap_or(false);

                match jobs
                    .output(&ctx.session_id, &id.to_string(), cursor, wait)
                    .await
                {
                    Ok(read) => Ok(render(&read)),
                    Err(why) => Ok(format!("Error: {why}")),
                }
            })
        }),
    }
}

/// `job_list`：这个会话起了哪些作业。
pub fn job_list(jobs: Option<std::sync::Arc<dyn JobRegistry>>) -> Tool {
    Tool {
        name: "job_list".to_string(),
        description: "列出这个会话起的后台作业（只有自己的）。".to_string(),
        input_schema: json!({ "type": "object", "properties": {} }),
        handler: std::sync::Arc::new(move |ctx: crate::ToolCtx, _args: Value| {
            let jobs = jobs.clone();
            Box::pin(async move {
                let Some(jobs) = jobs else {
                    return Ok(no_registry());
                };
                let list = jobs.list(&ctx.session_id);
                if list.is_empty() {
                    return Ok("这个会话还没有后台作业。".to_string());
                }
                let mut out = String::from("后台作业：\n");
                for job in list {
                    out.push_str(&format!(
                        "- {} {}（{} 字节）: {}\n",
                        job.id,
                        match job.exit_code {
                            Some(code) => format!("已结束（退出码 {code}）"),
                            None => "仍在跑".to_string(),
                        },
                        job.bytes,
                        job.command
                    ));
                }
                Ok(out.trim_end().to_string())
            })
        }),
    }
}

/// `job_kill`：停掉它。
pub fn job_kill(jobs: Option<std::sync::Arc<dyn JobRegistry>>) -> Tool {
    Tool {
        name: "job_kill".to_string(),
        description: "停掉一个后台作业（连带它的进程组）。".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["id"],
            "properties": { "id": { "type": "string" } }
        }),
        handler: std::sync::Arc::new(move |ctx: crate::ToolCtx, args: Value| {
            let jobs = jobs.clone();
            Box::pin(async move {
                let Some(jobs) = jobs else {
                    return Ok(no_registry());
                };
                let Some(id) = args.get("id").and_then(Value::as_str) else {
                    return Ok("Error: 缺少 id".to_string());
                };
                match jobs.kill(&ctx.session_id, &id.to_string()).await {
                    Ok(true) => Ok(format!("已停掉 job {id}。")),
                    Ok(false) => Ok(format!("job {id} 已经结束了，没什么可停的。")),
                    Err(why) => Ok(format!("Error: {why}")),
                }
            })
        }),
    }
}

/// 把一截输出渲染成给模型看的文本。
fn render(read: &JobRead) -> String {
    let mut out = String::new();
    if read.lossy {
        out.push_str(&format!(
            "（前面的 {} 字节已经被丢掉了：输出太多，环里只留了最近的）\n",
            read.dropped
        ));
    }
    if read.running {
        out.push_str(&format!("job 仍在跑（已读到 {}）", read.cursor));
    } else {
        out.push_str(&format!(
            "job 已结束（退出码 {}）",
            read.exit_code.map(|c| c.to_string()).unwrap_or("?".into())
        ));
    }
    out.push('\n');
    if read.text.is_empty() {
        out.push_str("（这一段没有新输出）\n");
    } else {
        out.push_str(&read.text);
        if !read.text.ends_with('\n') {
            out.push('\n');
        }
    }
    if read.running {
        out.push_str(&format!("（继续读就带上 cursor={}）", read.cursor));
    }
    out
}
