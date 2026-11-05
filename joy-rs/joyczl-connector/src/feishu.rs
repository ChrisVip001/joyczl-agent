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
        let guard = self.inner.lock().expect("token 锁不该中毒");
        guard
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(100 * 60))
            .map(|(token, _)| token.clone())
    }

    fn put(&self, token: String) {
        *self.inner.lock().expect("token 锁不该中毒") = Some((token, Instant::now()));
    }
}

pub fn feishu_send_tool(config: FeishuConfig) -> Tool {
    let cache = Arc::new(TokenCache::default());
    Tool {
        name: "feishu_send".to_string(),
        description: "给白名单里的飞书会话或成员发一条文本消息。默认只写进本地 outbox 当草稿；\
                      真正发送需要配置里开启 live，并且每次都会被问一次。"
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "to": {"type": "string", "description": "接收方 id（open_id 或 chat_id，必须在白名单内）"},
                "text": {"type": "string", "description": "消息正文（纯文本）"},
                "dry_run": {"type": "boolean", "description": "只留草稿、不发。不填则按配置"}
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
            "接收方 {to} 不在白名单里。\n\
             白名单在 {} 的 allow_recipients（飞书段）—— 只有人能改；留空就是谁都不许。",
            ctx.home.join("connectors.json").display()
        );
    }

    // Fence 2: drafts by default.
    if dry_run || !config.live {
        let path = write_draft(&ctx.home, &config, &to, &text).await?;
        return Ok(format!(
            "飞书消息草稿已放进 outbox（{}）：发给 {to}。{}",
            path.display(),
            if config.live {
                "这次是你的 dry_run 要求"
            } else {
                "连接器还没开 live，不会真的发"
            }
        ));
    }

    // Credentials before the card — no wasted click.
    let Some(secret) = config.secret(&ctx.home) else {
        bail!(
            "飞书 app_secret 找不到（{}）—— 先在钥匙串或环境变量里补上。",
            config.app_secret
        );
    };

    // Fence 3: one approval per live send, naming the target and previewing the text.
    let Some(broker) = ctx.approval.clone() else {
        bail!("要真的发飞书消息必须先问人，但当前没有可应答的界面 —— 这次没有发送。");
    };
    let request = ApprovalRequest {
        tool: "feishu_send".to_string(),
        args_preview: format!("发给 {to}：{}", summarize("飞书消息", &text)),
        reason: "连接器已开启 live，每次实际发送都会确认".to_string(),
        timeout_secs: APPROVAL_TIMEOUT_SECS,
    };
    if !broker.request(request).await {
        bail!("这次发送没有被批准（或没人回答），已跳过。");
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
    Ok(format!("已发给 {to}（飞书 message_id {message_id}）。"))
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
        .context("连不上飞书鉴权端点")?;
    let body: Value = response.json().await.context("飞书鉴权端点没回 JSON")?;
    if body.get("code").and_then(Value::as_i64) != Some(0) {
        bail!(
            "飞书拒绝发 token：{} {}（检查 app_id / app_secret 与应用的可用范围）",
            body.get("code").unwrap_or(&Value::Null),
            body.get("msg").and_then(Value::as_str).unwrap_or("")
        );
    }
    body.get("tenant_access_token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("飞书应答里没有 tenant_access_token：{body}"))
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
        .context("连不上飞书消息端点")?;
    let body: Value = response.json().await.context("飞书消息端点没回 JSON")?;
    if body.get("code").and_then(Value::as_i64) != Some(0) {
        bail!(
            "飞书拒绝发送：{} {}",
            body.get("code").unwrap_or(&Value::Null),
            body.get("msg").and_then(Value::as_str).unwrap_or("")
        );
    }
    Ok(body
        .get("data")
        .and_then(|data| data.get("message_id"))
        .and_then(Value::as_str)
        .unwrap_or("(没有 id)")
        .to_string())
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent(concat!("joy/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("建不了 HTTP 客户端")
}

/// Send for real, with the token cache bypassed (CLI: one message, one process).
pub async fn send_live(
    config: &FeishuConfig,
    home: &std::path::Path,
    to: &str,
    text: &str,
) -> Result<String> {
    let Some(secret) = config.secret(home) else {
        bail!("飞书 app_secret 找不到（{}）。", config.app_secret);
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
