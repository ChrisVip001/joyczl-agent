//! Background jobs: a long command shouldn't hold a whole conversation turn.
//!
//! A `cargo test` takes three minutes. Foreground `run_command` would **block the turn**:
//! a frozen UI, and nothing else entering the context. Background jobs split the two: the
//! command runs in its own process, output goes into a bounded ring the model reads on demand.
//!
//! Three rules:
//!
//! * **Authorization via the owner fence, not id secrecy**: a job id is a predictable short
//!   string, but only **the spawning session** can read or stop it; another session gets nothing.
//! * **Output is a bounded ring**: overflow drops the oldest bytes; reads **honestly report
//!   how much was lost** instead of erroring — a runaway command must not eat memory or fail reads.
//! * **Same gate as foreground**: background commands still pass vet and approval (exec.rs's
//!   `pass_gate`) — "background it and run at leisure" is the most natural way to bypass approval.
//!
//! Lifecycle: jobs live and die with the Joy process; children are reaped (`kill_on_drop`),
//! so "resume after restart" is meaningless here — not a daemon, just this run's helper.

use serde_json::{json, Value};

use crate::Tool;

/// A job's id (a predictable short string; see module docs).
pub type JobId = String;

/// A read of output.
#[derive(Debug, Clone)]
pub struct JobRead {
    /// New output starting from `cursor`.
    pub text: String,
    /// Where to resume next time.
    pub cursor: u64,
    pub running: bool,
    /// The ring overflowed: the bytes `dropped` mentions are gone.
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

/// The host capability for background jobs. The tool layer knows only this trait; the
/// implementation (process + ring + spill) lives in app-server — same discipline as `SubagentRunner`.
pub type JobFut<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;

pub trait JobRegistry: Send + Sync {
    /// Spawn a job. `owner` is the session name (the authorization fence); `cwd` means the
    /// same as foreground `run_command`'s `cwd` (writable roots are computed from it).
    fn spawn(
        &self,
        owner: &str,
        command: &str,
        cwd: Option<String>,
    ) -> JobFut<Result<JobId, String>>;
    /// Read a slice. With `wait` true, wait a bounded while (until a result or the cap).
    fn output(
        &self,
        owner: &str,
        id: &JobId,
        cursor: u64,
        wait: bool,
    ) -> JobFut<Result<JobRead, String>>;
    /// Stop it. Returns "was it still running".
    fn kill(&self, owner: &str, id: &JobId) -> JobFut<Result<bool, String>>;
    /// This session's own jobs (nobody else's).
    fn list(&self, owner: &str) -> Vec<JobSummary>;
}

/// The answer to the model when no job host is present (background is off by default).
fn no_registry() -> String {
    "Error: 这台机器上没有开启后台作业（它跟着 run_command 一起开关：JOY_EXEC=1）。".to_string()
}

/// `job_output`: read a slice of output. To continue reading, pass back the `cursor` you got.
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

/// `job_list`: which jobs this session spawned.
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

/// `job_kill`: stop it.
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

/// Render a slice of output into text for the model.
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
