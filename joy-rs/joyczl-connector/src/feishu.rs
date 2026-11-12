//! `feishu_send` — the second connector: a message into a Feishu/Lark chat or to a person.
//!
//! Same three fences as email (allowlist → live switch → one approval per live send).
//! What differs is the transport: an app-identity `tenant_access_token`, fetched on
//! demand and cached **in memory only** — it expires in two hours and is derived from
//! the app secret, so writing another credential to disk would buy nothing.
//!
//! Identity boundary, stated plainly: messages go out **as the app**, which is what a
//! custom app can do. Acting as a specific user needs Feishu's OAuth code flow with a
//! console-registered redirect URI; that lands when there is a real app to verify it
//! against (see docs/limitations).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use joyczl_tools::approval::ApprovalRequest;
use joyczl_tools::{require_str, Tool, ToolCtx};
use serde_json::{json, Value};

use crate::config::FeishuConfig;
use crate::email::summarize;

/// Approval wait, same reasoning as email.
const APPROVAL_TIMEOUT_SECS: i64 = 120;
/// A message API that takes 20s is down; don't hold the turn hostage.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// In-process token cache: (token, fetched_at). Public because the tool closure shares it
/// across turns — a token per turn would hammer the auth endpoint for no reason.
#[derive(Default)]
pub struct TokenCache {
    inner: Mutex<Option<(String, Instant)>>,
}

impl TokenCache {
    fn get(&self) -> Option<String> {
        let guard = self
            .inner
            .lock()
            .expect("the token lock should not be poisoned");
        guard
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(100 * 60))
            .map(|(token, _)| token.clone())
    }

    fn put(&self, token: String) {
        *self
            .inner
            .lock()
            .expect("the token lock should not be poisoned") = Some((token, Instant::now()));
    }
}

pub fn feishu_send_tool(config: FeishuConfig) -> Tool {
    let cache = Arc::new(TokenCache::default());
    Tool {
        name: "feishu_send".to_string(),
        description: "Send a text message to an allowlisted Feishu chat or member. By default it only writes a draft into the local outbox; actually sending needs live turned on in the config, and every send asks first."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "to": {"type": "string", "description": "recipient id (open_id or chat_id, must be allowlisted)"},
                "text": {"type": "string", "description": "message body (plain text)"},
                "dry_run": {"type": "boolean", "description": "keep a draft, do not send. Unset = follow the config"}
            },
            "required": ["to", "text"]
        }),
        handler: Arc::new(move |ctx: ToolCtx, args: Value| {
            let config = config.clone();
            let cache = cache.clone();
            Box::pin(async move { run(ctx, config, cache, args).await })
        }),
    }
}

async fn run(
    ctx: ToolCtx,
    config: FeishuConfig,
    cache: Arc<TokenCache>,
    args: Value,
) -> Result<String> {
    let to = require_str(&args, "to")?.trim().to_string();
    let text = require_str(&args, "text")?;
    let dry_run = args
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Fence 1: allowlist, fail closed.
    if !config.permits(&to) {
        bail!(
            "Recipient {to} is not on the allowlist.\n\n             The allowlist is allow_recipients in {} (feishu section) — only a person can change it; empty means nobody is allowed.",
            ctx.home.join("connectors.json").display()
        );
    }

    // Fence 2: drafts by default.
    if dry_run || !config.live {
        let path = write_draft(&ctx.home, &config, &to, &text).await?;
        return Ok(format!(
            "Feishu draft put in the outbox ({}): to {to}.{}",
            path.display(),
            if config.live {
                " this run's dry_run asked for it"
            } else {
                "; the connector is not live, so nothing was sent"
            }
        ));
    }

    // Credentials before the card — no wasted click.
    let Some(secret) = config.secret(&ctx.home) else {
        bail!(
            "the Feishu app_secret could not be found ({}) — put it in the keychain or an environment variable first.",
            config.app_secret
        );
    };

    // Fence 3: one approval per live send, naming the target and previewing the text.
    let Some(broker) = ctx.approval.clone() else {
        bail!("Sending a Feishu message for real would have to ask a person first, and there is no interface to answer — nothing was sent this time.");
    };
    let request = ApprovalRequest {
        tool: "feishu_send".to_string(),
        args_preview: format!("To {to}: {}", summarize("Feishu message", &text)),
        reason: "the connector is live; every actual send is confirmed".to_string(),
        timeout_secs: APPROVAL_TIMEOUT_SECS,
        // Nothing worth remembering: a live send is a one-off act, not a standing rule.
        remember: None,
    };
    if !broker.request(request).await {
        bail!("This send was not approved (or nobody answered); skipped.");
    }

    let token = match config.identity().unwrap_or("app") {
        // Acting as a person: no cache — the token file is the cache, and refresh is
        // feishu_oauth's business.
        "user" => crate::feishu_oauth::access_token(&config, &ctx.home).await?,
        _ => match cache.get() {
            Some(token) => token,
            None => {
                let token = tenant_token(&config, &secret).await?;
                cache.put(token.clone());
                token
            }
        },
    };
    let message_id = send_message(&config, &token, &to, &text).await?;
    Ok(format!("Sent to {to} (Feishu message_id {message_id})."))
}

/// The app-identity token. `code != 0` is a real answer from Feishu, quote it verbatim.
async fn tenant_token(config: &FeishuConfig, secret: &str) -> Result<String> {
    let response = client()?
        .post(format!(
            "{}/open-apis/auth/v3/tenant_access_token/internal",
            config.base_url.trim_end_matches('/')
        ))
        .json(&json!({ "app_id": config.app_id, "app_secret": secret }))
        .send()
        .await
        .context("could not reach the Feishu auth endpoint")?;
    let body: Value = response
        .json()
        .await
        .context("the Feishu auth endpoint did not return JSON")?;
    if body.get("code").and_then(Value::as_i64) != Some(0) {
        bail!(
            "Feishu refused to issue a token: {} {} (check app_id / app_secret and the app's availability scope)",
            body.get("code").unwrap_or(&Value::Null),
            body.get("msg").and_then(Value::as_str).unwrap_or("")
        );
    }
    body.get("tenant_access_token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("the Feishu reply has no tenant_access_token: {body}"))
}

async fn send_message(config: &FeishuConfig, token: &str, to: &str, text: &str) -> Result<String> {
    let url = format!(
        "{}/open-apis/im/v1/messages?receive_id_type={}",
        config.base_url.trim_end_matches('/'),
        config.receive_id_type
    );
    // `content` is a JSON **string** per Feishu's API — a nested object is rejected.
    let content = json!({ "text": text }).to_string();
    let response = client()?
        .post(url)
        .bearer_auth(token)
        .json(&json!({ "receive_id": to, "msg_type": "text", "content": content }))
        .send()
        .await
        .context("could not reach the Feishu message endpoint")?;
    let body: Value = response
        .json()
        .await
        .context("the Feishu message endpoint did not return JSON")?;
    if body.get("code").and_then(Value::as_i64) != Some(0) {
        bail!(
            "Feishu refused the send: {} {}",
            body.get("code").unwrap_or(&Value::Null),
            body.get("msg").and_then(Value::as_str).unwrap_or("")
        );
    }
    Ok(body
        .get("data")
        .and_then(|data| data.get("message_id"))
        .and_then(Value::as_str)
        .unwrap_or("(no id)")
        .to_string())
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent(concat!("joy/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("could not build the HTTP client")
}

/// Send for real, with the token cache bypassed (CLI: one message, one process).
pub async fn send_live(
    config: &FeishuConfig,
    home: &std::path::Path,
    to: &str,
    text: &str,
) -> Result<String> {
    let Some(secret) = config.secret(home) else {
        bail!(
            "the Feishu app_secret could not be found ({}).",
            config.app_secret
        );
    };
    let token = match config.identity().unwrap_or("app") {
        "user" => crate::feishu_oauth::access_token(config, home).await?,
        _ => tenant_token(config, &secret).await?,
    };
    send_message(config, &token, to, text).await
}

pub async fn write_draft(
    home: &std::path::Path,
    config: &FeishuConfig,
    to: &str,
    text: &str,
) -> Result<std::path::PathBuf> {
    let dir = home.join("outbox");
    tokio::fs::create_dir_all(&dir).await?;
    let stamp = Utc::now().format("%Y%m%dT%H%M%S");
    let safe_to: String = to
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .take(40)
        .collect();
    let path = dir.join(format!("feishu-{stamp}-{safe_to}.txt"));
    let kind = if config.receive_id_type == "chat_id" {
        "chat_id"
    } else {
        "open_id"
    };
    let text = format!("To: {to} ({kind})\nApp: {}\n\n{text}\n", config.app_id);
    tokio::fs::write(&path, text).await?;
    Ok(path)
}
