//! send_message — drafts only, never actually sends.
//!
//! The local-first hard rule: every "message" becomes a file in outbox/, which the user
//! reads, edits, and sends themselves. Wiring up real channels (email, Slack) is a good
//! community contribution — until then, an assistant that sends for you is a hazard.

use std::sync::Arc;

use chrono::Utc;
use serde_json::{json, Value};

use crate::{require_str, Tool, ToolCtx};

pub fn send_message() -> Tool {
    Tool {
        name: "send_message".to_string(),
        description: "给某人起草一条消息，放进本地 outbox 供用户审阅后自己发送。\
                      用户让你「给谁发个消息/转告/提醒」时用。**从不真的发送。**"
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "to": {"type": "string", "description": "收件人名字或地址"},
                "body": {"type": "string", "description": "消息正文"}
            },
            "required": ["to", "body"]
        }),
        handler: Arc::new(|ctx: ToolCtx, args: Value| {
            Box::pin(async move {
                let to = require_str(&args, "to")?;
                let body = require_str(&args, "body")?;

                let stamp = Utc::now().format("%Y%m%dT%H%M%S");
                // The recipient becomes part of the filename: keep only alphanumerics,
                // replace the rest with -, so a slash or colon cannot traverse directories.
                let safe_to: String = to
                    .chars()
                    .map(|c| if c.is_alphanumeric() { c } else { '-' })
                    .take(40)
                    .collect();
                let dir = ctx.home.join("outbox");
                tokio::fs::create_dir_all(&dir).await?;
                let path = dir.join(format!("{stamp}-{safe_to}.txt"));
                tokio::fs::write(&path, format!("To: {to}\n\n{body}\n")).await?;

                Ok(format!(
                    "给 {to} 的消息已放进 outbox（{}）。没有真的发送 —— 请到那里审阅后自己发。",
                    path.display()
                ))
            })
        }),
    }
}
