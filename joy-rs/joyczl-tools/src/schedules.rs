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
        Ok(when) => format!("Next run {when}"),
        Err(why) => format!("({why}, so it will not run)"),
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
        description: "Create a scheduled job: when (five-field cron) and what (one sentence). At that time it runs as a turn and the result goes to <home>/outbox/. The thing that actually fires is the `joy schedule` daemon — if it is not running, nothing runs. Use it when the user says things like \"send me a briefing at eight every day\"; do not hand-write schedules.json."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "The job name: lowercase letters, digits, hyphens, e.g. morning-news"},
                "cron": {"type": "string", "description": "Five-field cron: minute hour day month weekday, e.g. `0 8 * * 1-5` for 08:00 on weekdays"},
                "prompt": {"type": "string", "description": "The sentence sent to Joy when it fires"},
                "delivery": {
                    "type": "string",
                    "enum": ["outbox", "notify", "quiet"],
                    "description": "outbox (default, writes a file) / notify (file + notification) / quiet (log only)"
                },
                "catch_up": {"type": "boolean", "description": "Catch up once after a missed run? Default true"}
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
                    "{summary}.{}.\n\n                     Firing is done by the `joy schedule` daemon: nothing runs unless it is running (the cockpit shows this job under Capabilities → Scheduled jobs).",
                    next_label(&cron)
                ))
            })
        }),
    }
}

fn list_schedules() -> Tool {
    Tool {
        name: "list_schedules".to_string(),
        description: "List the scheduled jobs (name, cron, prompt, delivery). Use it when the user asks what is scheduled. Last and next run times are in the cockpit."
            .to_string(),
        input_schema: json!({"type": "object", "properties": {}, "required": []}),
        handler: Arc::new(|ctx: ToolCtx, _args: Value| {
            Box::pin(async move {
                let (file_jobs, error) = declarations::read(&ctx.home);
                let mut lines: Vec<String> = Vec::new();
                for skill in joyczl_memory::skills::loaded_skills(&ctx.home) {
                    let Some(cron) = skill.schedule else { continue };
                    lines.push(format!(
                        "- {} (comes with a skill) · {cron} · runs that skill's method",
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
                        "No scheduled jobs yet. To add one, tell me when (cron) and what.".to_string()
                    );
                }
                let mut out = format!("{} scheduled jobs:\n{}", lines.len(), lines.join("\n"));
                if let Some(why) = error {
                    out.push_str(&format!("\nNote: {why}"));
                }
                out.push_str("\nFiring is done by the `joy schedule` daemon; nothing runs unless it is running.");
                Ok(out)
            })
        }),
    }
}

fn cancel_schedule() -> Tool {
    Tool {
        name: "cancel_schedule".to_string(),
        description:
            "Delete a scheduled job (by name; only the ones in schedules.json). Use it when the user says \"cancel that job\"; a job that comes with a skill has to be changed in its SKILL.md, and this tool refuses and says so."
                .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "The job name (check with list_schedules first)"}
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
