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
        description: "Send mail to an allowlisted recipient. By default it only writes a draft into the local outbox (nothing is sent); actually sending needs live turned on in the config, and every send asks first."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "to": {"type": "string", "description": "recipient addresses, comma-separated (must be allowlisted)"},
                "subject": {"type": "string", "description": "subject"},
                "body": {"type": "string", "description": "body (plain text)"},
                "dry_run": {"type": "boolean", "description": "keep a draft, do not send. Unset = follow the config (also a draft by default)"}
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
        bail!("no usable address in to");
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
            "Mail draft put in the outbox ({}): recipients {}.{}",
            path.display(),
            to.join(", "),
            if config.live {
                " this run's dry_run asked for it"
            } else {
                "; the connector is not live, so nothing was sent"
            }
        ));
    }

    // Validate the credentials **before** the card: asking a human to approve and then
    // failing on a missing password wastes the one click they were willing to give.
    let password = password_of(&config, &ctx.home)?;

    // Fence 3: every live send asks once, and the card says what will happen.
    let Some(broker) = ctx.approval.clone() else {
        bail!("Sending for real would have to ask a person first, and there is no interface to answer — nothing was sent this time.");
    };
    let request = ApprovalRequest {
        tool: "email_send".to_string(),
        args_preview: format!("To {}: {}", to.join(", "), summarize(&subject, &body)),
        reason: "the connector is live; every actual send is confirmed".to_string(),
        timeout_secs: APPROVAL_TIMEOUT_SECS,
        // Nothing worth remembering: a live send is a one-off act, not a standing rule.
        remember: None,
    };
    if !broker.request(request).await {
        bail!("This send was not approved (or nobody answered); skipped.");
    }

    send_live(&config, password, &message).await?;
    Ok(format!(
        "Sent to {} through {}:{}.",
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
        "These recipients are not on the allowlist: {}.\n\n         The allowlist is allow_recipients in {} — only a person can change it; empty means nobody is allowed.",
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
            "An SMTP user name is configured, but the password the secret points at does not exist ({}) — put it in the keychain or an environment variable first.",
            config.secret.as_deref().unwrap_or("(no secret configured)")
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
        format!("\"{subject}\" body: {preview}…")
    } else {
        format!("\"{subject}\" body: {preview}")
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
