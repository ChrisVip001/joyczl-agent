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
        description: "Draft a message for someone, into the local outbox for the user to review and send themselves. Use it when the user says \"send X a message / pass this on / remind them\". **It never actually sends.**"
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "to": {"type": "string", "description": "the recipient's name or address"},
                "body": {"type": "string", "description": "the message body"}
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
                    "The message for {to} is in the outbox ({}). Nothing was sent — review it there and send it yourself.",
                    path.display()
                ))
            })
        }),
    }
}
