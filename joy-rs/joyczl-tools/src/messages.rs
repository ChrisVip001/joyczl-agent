//! send_message —— 只出草稿，从不真发。
//!
//! local-first 的硬规矩：每条「消息」变成 outbox/ 里的一个文件，
//! 用户读过、改过、自己发。接真渠道（邮件、Slack）是个好社区贡献 ——
//! 但在那之前，一个能替用户发消息的助理是个安全隐患。

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
                // 收件人变成文件名的一部分：只留字母数字，其余换成 -，
                // 名字里带斜杠、冒号也翻不出目录。
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
