//! `email_send` — the first connector, and the template for the rest.
//!
//! Three fences, in order, before a byte leaves the machine:
//!
//!   1. **Recipient allowlist** (hard boundary) — an empty list allows nobody;
//!   2. **Live switch** — `"live": true` in connectors.json, otherwise a draft in outbox/;
//!   3. **One approval per live send**, naming recipients and previewing the body.
//!
//! The model can trigger a send, but it cannot widen any of these: they all live in
//! files only the human edits.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Result};
use chrono::Utc;
use joyczl_tools::approval::ApprovalRequest;
use joyczl_tools::{require_str, Tool, ToolCtx};
use serde_json::{json, Value};

use crate::config::EmailConfig;
use crate::smtp::{self, SmtpConfig};
use crate::Outgoing;

/// How long an approval card waits for a human. Long enough to read a body preview,
/// short enough that a walked-away user doesn't pin a turn forever.
const APPROVAL_TIMEOUT_SECS: i64 = 120;

pub fn email_send_tool(config: EmailConfig) -> Tool {
    Tool {
        name: "email_send".to_string(),
        description: "给白名单里的收件人发邮件。默认只写进本地 outbox 当草稿（不发）；\
                      真正发送需要用户在配置里开启 live，并且每次都会被问一次。"
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "to": {"type": "string", "description": "收件人地址，多个用逗号分隔（必须在白名单内）"},
                "subject": {"type": "string", "description": "主题"},
                "body": {"type": "string", "description": "正文（纯文本）"},
                "dry_run": {"type": "boolean", "description": "只留草稿、不发。不填则按配置（默认也是草稿）"}
            },
            "required": ["to", "subject", "body"]
        }),
        handler: Arc::new(move |ctx: ToolCtx, args: Value| {
            let config = config.clone();
            Box::pin(async move { run(ctx, config, args).await })
        }),
    }
}

async fn run(ctx: ToolCtx, config: EmailConfig, args: Value) -> Result<String> {
    let to = recipients(&require_str(&args, "to")?);
    let subject = require_str(&args, "subject")?;
    let body = require_str(&args, "body")?;
    let dry_run = args
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if to.is_empty() {
        bail!("to 里没有可用地址");
    }

    // Fence 1: the allowlist is a hard boundary, and it fails closed.
    check_recipients(&config, &to, &ctx.home)?;

    let message = Outgoing {
        from: config.from.clone(),
        to: to.clone(),
        subject: subject.clone(),
        body: body.clone(),
    };

    // Fence 2: drafts by default. Drafting is not an external action, so it asks nothing.
    if dry_run || !config.live {
        let path = write_draft(&ctx.home, &message).await?;
        return Ok(format!(
            "邮件草稿已放进 outbox（{}）：收件人 {}。{}",
            path.display(),
            to.join(", "),
            if config.live {
                "这次是你的 dry_run 要求"
            } else {
                "连接器还没开 live，不会真的发"
            }
        ));
    }

    // Validate the credentials **before** the card: asking a human to approve and then
    // failing on a missing password wastes the one click they were willing to give.
    let password = password_of(&config, &ctx.home)?;

    // Fence 3: every live send asks once, and the card says what will happen.
    let Some(broker) = ctx.approval.clone() else {
        bail!("要真的发邮件必须先问人，但当前没有可应答的界面 —— 这次没有发送。");
    };
    let request = ApprovalRequest {
        tool: "email_send".to_string(),
        args_preview: format!("发给 {}：{}", to.join(", "), summarize(&subject, &body)),
        reason: "连接器已开启 live，每次实际发送都会确认".to_string(),
        timeout_secs: APPROVAL_TIMEOUT_SECS,
    };
    if !broker.request(request).await {
        bail!("这次发送没有被批准（或没人回答），已跳过。");
    }

    send_live(&config, password, &message).await?;
    Ok(format!(
        "已通过 {}:{} 发给 {}。",
        config.smtp_host,
        config.smtp_port,
        to.join(", ")
    ))
}

/// The allowlist check, shared by the tool (model-driven) and the CLI (human-driven).
/// Fails closed: an empty list allows nobody, and the error says where to fix it.
pub fn check_recipients(config: &EmailConfig, to: &[String], home: &std::path::Path) -> Result<()> {
    let rejected: Vec<&String> = to.iter().filter(|addr| !config.permits(addr)).collect();
    if rejected.is_empty() {
        return Ok(());
    }
    bail!(
        "这些收件人不在白名单里：{}。\n\
         白名单在 {} 的 allow_recipients —— 只有人能改；留空就是谁都不许。",
        rejected
            .iter()
            .map(|addr| addr.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        home.join("connectors.json").display()
    )
}

/// Resolve the SMTP password up front (keychain / env), with a message naming the source.
pub fn password_of(config: &EmailConfig, home: &std::path::Path) -> Result<Option<String>> {
    let password = {
        let mut config = config.clone();
        config.password(home)
    };
    if config.username.is_some() && password.is_none() {
        bail!(
            "配了 SMTP 用户名，但 secret 指向的密码不存在（{}）—— 先在钥匙串或环境变量里补上。",
            config.secret.as_deref().unwrap_or("(没配 secret)")
        );
    }
    Ok(password)
}

/// The one place a socket is opened. Callers have already passed the fences.
pub async fn send_live(
    config: &EmailConfig,
    password: Option<String>,
    message: &Outgoing,
) -> Result<()> {
    let smtp = SmtpConfig {
        host: config.smtp_host.clone(),
        port: config.smtp_port,
        from: config.from.clone(),
        username: config.username.clone(),
        password,
    };
    smtp::send(&smtp, message).await
}

/// Commas or semicolons, trimmed, empties dropped. Addresses are not validated beyond
/// "has an @": the SMTP server is the authority, and the allowlist already gated them.
fn recipients(raw: &str) -> Vec<String> {
    raw.split([',', ';'])
        .map(str::trim)
        .filter(|addr| !addr.is_empty())
        .map(str::to_string)
        .collect()
}

/// One line for the approval card: subject plus as much body as a person can judge in
/// a glance. The full body goes to the draft file when it doesn't fit.
pub(crate) fn summarize(subject: &str, body: &str) -> String {
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let preview: String = flat.chars().take(160).collect();
    if preview.len() < flat.len() {
        format!("「{subject}」正文：{preview}…")
    } else {
        format!("「{subject}」正文：{preview}")
    }
}

pub async fn write_draft(home: &std::path::Path, message: &Outgoing) -> Result<PathBuf> {
    let dir = home.join("outbox");
    tokio::fs::create_dir_all(&dir).await?;
    let stamp = Utc::now().format("%Y%m%dT%H%M%S");
    // The recipient is part of the filename: keep it boring so a slash cannot escape.
    let safe_to: String = message
        .to
        .join("-")
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .take(40)
        .collect();
    let path = dir.join(format!("email-{stamp}-{safe_to}.txt"));
    let text = format!(
        "To: {}\nFrom: {}\nSubject: {}\n\n{}\n",
        message.to.join(", "),
        message.from,
        message.subject,
        message.body
    );
    tokio::fs::write(&path, text).await?;
    Ok(path)
}
