//! Scheduled jobs as tools: the model can create, list and cancel one from a sentence.
//!
//! Why a tool instead of "just write the file": `<home>/schedules.json` sits outside every
//! workspace, so `write_file` refuses it and the human is asked to pick a working directory —
//! for a file that has nothing to do with their project. A job declaration is Joy's own state,
//! not project content, so the tool that owns it is the honest door. (This is the same reason
//! `save_note` exists next to `write_file`.)
//!
//! What these tools do **not** do is start a scheduler: `joy schedule` still fires the jobs,
//! and every answer says so. A tool that looks like it scheduled work but quietly needs a
//! resident process would be worse than no tool at all.

use std::sync::Arc;

use joyczl_schedule::declarations::{self, JobBody};
use serde_json::{json, Value};

use crate::{require_str, Tool, ToolCtx};

/// The next fire, or the sentence saying why there is none.
fn next_label(cron: &str) -> String {
    let now = chrono::Local::now().naive_local();
    match declarations::next_run_at(cron, now) {
        Ok(when) => format!("下次触发 {when}"),
        Err(why) => format!("（{why}，所以它不会跑）"),
    }
}

/// Skill-declared job names — a file job may not shadow one.
fn reserved(home: &std::path::Path) -> Vec<String> {
    joyczl_memory::skills::loaded_skills(home)
        .into_iter()
        .filter(|skill| skill.schedule.is_some())
        .map(|skill| skill.name)
        .collect()
}

fn body_from(args: &Value) -> JobBody {
    JobBody {
        name: args
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        cron: args
            .get("cron")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        prompt: args
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        delivery: args
            .get("delivery")
            .and_then(Value::as_str)
            .unwrap_or("outbox")
            .to_string(),
        catch_up: args.get("catch_up").and_then(Value::as_bool),
    }
}

pub fn schedule_tools() -> Vec<Tool> {
    vec![schedule_job(), list_schedules(), cancel_schedule()]
}

fn schedule_job() -> Tool {
    Tool {
        name: "schedule_job".to_string(),
        description: "创建一个定时任务：什么时候（五段 cron）、做什么（一句话）。到点会在一轮\
                      对话里执行，结果写进 <home>/outbox/。真正触发的是 `joy schedule` 这个\
                      常驻进程 —— 如果它没在跑，任务不会执行。用户说「每天八点给我简报」\
                      这类话时用它，不要去手写 schedules.json。"
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "任务名：小写字母、数字、连字符，如 morning-news"},
                "cron": {"type": "string", "description": "五段 cron：分 时 日 月 周。如 `0 8 * * 1-5` 工作日八点"},
                "prompt": {"type": "string", "description": "触发时发给 Joy 的那一句话"},
                "delivery": {
                    "type": "string",
                    "enum": ["outbox", "notify", "quiet"],
                    "description": "outbox（默认，写文件）/ notify（文件 + 通知）/ quiet（只记日志）"
                },
                "catch_up": {"type": "boolean", "description": "错过之后补跑一次？默认 true"}
            },
            "required": ["name", "cron", "prompt"]
        }),
        handler: Arc::new(|ctx: ToolCtx, args: Value| {
            Box::pin(async move {
                let body = body_from(&args);
                let summary = declarations::save(&ctx.home, &body, &reserved(&ctx.home))
                    .map_err(|why| anyhow::anyhow!(why))?;
                let cron = body.cron.split_whitespace().collect::<Vec<_>>().join(" ");
                Ok(format!(
                    "{summary}。{}。\n\
                     到点执行的是 `joy schedule` 常驻进程：它没在跑就不会触发（界面上「能力中心 → 定时任务」能看到这一行）。",
                    next_label(&cron)
                ))
            })
        }),
    }
}

fn list_schedules() -> Tool {
    Tool {
        name: "list_schedules".to_string(),
        description: "列出已有的定时任务（名字、cron、提示词、投递方式）。用户问「我都有哪些\
                      定时任务」时用。上次/下次什么时候跑要看驾驶舱。"
            .to_string(),
        input_schema: json!({"type": "object", "properties": {}, "required": []}),
        handler: Arc::new(|ctx: ToolCtx, _args: Value| {
            Box::pin(async move {
                let (file_jobs, error) = declarations::read(&ctx.home);
                let mut lines: Vec<String> = Vec::new();
                for skill in joyczl_memory::skills::loaded_skills(&ctx.home) {
                    let Some(cron) = skill.schedule else { continue };
                    lines.push(format!(
                        "- {}（随技能）· {cron} · 按技能的做法执行",
                        skill.name
                    ));
                }
                for job in file_jobs {
                    lines.push(format!(
                        "- {} · {} · {}",
                        job.name,
                        job.cron,
                        job.prompt.chars().take(60).collect::<String>()
                    ));
                }
                if lines.is_empty() {
                    return Ok(
                        "还没有定时任务。要加一个，告诉我：什么时候（cron）做什么。".to_string()
                    );
                }
                let mut out = format!("定时任务 {} 个：\n{}", lines.len(), lines.join("\n"));
                if let Some(why) = error {
                    out.push_str(&format!("\n注意：{why}"));
                }
                out.push_str("\n到点由 `joy schedule` 常驻进程执行；它没在跑就不会触发。");
                Ok(out)
            })
        }),
    }
}

fn cancel_schedule() -> Tool {
    Tool {
        name: "cancel_schedule".to_string(),
        description:
            "删掉一个定时任务（按名字，只删 schedules.json 里的那些）。用户说\
                      「把那个任务取消掉」时用；随技能来的任务要改 SKILL.md，这个工具会拒绝并说明。"
                .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "任务名（先用 list_schedules 看清楚）"}
            },
            "required": ["name"]
        }),
        handler: Arc::new(|ctx: ToolCtx, args: Value| {
            Box::pin(async move {
                let name = require_str(&args, "name")?;
                declarations::remove(&ctx.home, &name, &reserved(&ctx.home))
                    .map_err(|why| anyhow::anyhow!(why))
            })
        }),
    }
}
