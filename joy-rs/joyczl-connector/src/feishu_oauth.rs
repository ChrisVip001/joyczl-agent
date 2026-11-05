//! Feishu **user identity** (`user_access_token`) — the other half of "as whom".
//!
//! The app identity (`tenant_access_token`) says "this app wrote to you". A user token
//! says "**I** wrote to you": it is the app acting on behalf of the person who logged in.
//! That difference is why it is never automatic — `connectors.json` says `"as": "user"`,
//! a human ran `joy connector feishu login`, and the token sits in a 0600 file.
//!
//! Flow (Feishu's authorization-code grant, no PKCE):
//!   1. open `{base}/open-apis/authen/v1/authorize?app_id=…&redirect_uri=…&state=…`
//!   2. a local 127.0.0.1:41766/callback catches `code` + `state`
//!   3. POST `{base}/open-apis/authen/v2/oauth/token` → access_token + refresh_token
//!   4. refresh before use when expired (refresh tokens last longer than access tokens)
//!
//! Redirect URI must be registered in the app console **exactly**; the message says so
//! when Feishu rejects the exchange.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::config::FeishuConfig;

pub const CALLBACK_PORT: u16 = 41766;
pub const CALLBACK_PATH: &str = "/callback";
/// How long the human has to finish the browser round trip.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const AUTH_DIR: &str = "connector-auth";

/// What the token file holds. `expires_at` is a unix timestamp in seconds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserToken {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
}

fn token_path(home: &Path) -> PathBuf {
    home.join(AUTH_DIR).join("feishu.json")
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Load the stored token, if any.
pub fn load(home: &Path) -> Option<UserToken> {
    let raw = std::fs::read_to_string(token_path(home)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Save with 0600 — same discipline as `mcp-auth/<name>.json`.
pub(crate) fn save(home: &Path, token: &UserToken) -> Result<()> {
    let path = token_path(home);
    std::fs::create_dir_all(path.parent().expect("有父目录"))?;
    let body = serde_json::to_vec_pretty(token)?;
    std::fs::write(&path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Remove the token. Returns whether there was one.
pub fn logout(home: &Path) -> Result<bool> {
    let path = token_path(home);
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(path)?;
    Ok(true)
}

/// A one-line status for `joy connector list`.
pub fn status(home: &Path) -> Option<(bool, i64)> {
    load(home).map(|token| {
        let expired = token.expires_at <= now();
        (expired, token.expires_at)
    })
}

/// The token to send with, refreshed when it is (about to be) stale.
///
/// Five minutes of slack: a token that expires mid-request is a 401 the user did not
/// cause and cannot explain.
pub async fn access_token(config: &FeishuConfig, home: &Path) -> Result<String> {
    let Some(token) = load(home) else {
        bail!(
            "还没以用户身份登录飞书：先跑 `joy connector feishu login`\n\
             （或者把 connectors.json 里 feishu.as 改回默认的 app）"
        );
    };
    if token.expires_at - 300 > now() {
        return Ok(token.access_token);
    }
    refresh(config, home, &token.refresh_token).await
}

async fn refresh(config: &FeishuConfig, home: &Path, refresh_token: &str) -> Result<String> {
    let Some(secret) = config.secret(home) else {
        bail!(
            "飞书 app_secret 找不到，续不了用户 token（{}）",
            config.app_secret
        );
    };
    let body = exchange(
        config,
        &json!({
            "grant_type": "refresh_token",
            "client_id": config.app_id,
            "client_secret": secret,
            "refresh_token": refresh_token,
        }),
    )
    .await
    .context("续期飞书用户 token 失败；重新跑 `joy connector feishu login`")?;
    save(home, &body)?;
    Ok(body.access_token)
}

/// The human's login: open the browser, catch the code, exchange it, store it.
/// Returns the token's expiry for the message.
pub async fn login(config: &FeishuConfig, home: &Path) -> Result<i64> {
    let Some(secret) = config.secret(home) else {
        bail!(
            "飞书 app_secret 找不到（{}），没法换 token",
            config.app_secret
        );
    };
    let state = random_state();
    let redirect = format!("http://127.0.0.1:{CALLBACK_PORT}{CALLBACK_PATH}");
    let url = format!(
        "{}/open-apis/authen/v1/authorize?app_id={}&redirect_uri={}&state={}",
        config.base_url.trim_end_matches('/'),
        percent(&config.app_id),
        percent(&redirect),
        state
    );

    let listener = TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .await
        .with_context(|| format!("端口 {CALLBACK_PORT} 被占了（可能上一次登录还开着）"))?;
    eprintln!("(joy) 浏览器里完成授权。打不开就把这个地址粘进去：\n{url}");
    let _ = open_browser(&url);

    let code = tokio::time::timeout(SIGN_IN_TIMEOUT, wait_for_code(listener, &state))
        .await
        .map_err(|_| anyhow::anyhow!("等了 {}s 没等到授权，放弃了", SIGN_IN_TIMEOUT.as_secs()))??;

    let body = exchange(
        config,
        &json!({
            "grant_type": "authorization_code",
            "client_id": config.app_id,
            "client_secret": secret,
            "code": code,
            "redirect_uri": redirect,
        }),
    )
    .await?;
    save(home, &body)?;
    Ok(body.expires_at)
}

async fn exchange(config: &FeishuConfig, payload: &Value) -> Result<UserToken> {
    let response = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent(concat!("joy/", env!("CARGO_PKG_VERSION")))
        .build()?
        .post(format!(
            "{}/open-apis/authen/v2/oauth/token",
            config.base_url.trim_end_matches('/')
        ))
        .json(payload)
        .send()
        .await
        .context("连不上飞书授权端点")?;
    let body: Value = response.json().await.context("飞书授权端点没回 JSON")?;
    // v2 answers 0 on success; some deployments answer without a code at all.
    if let Some(code) = body.get("code").and_then(Value::as_i64) {
        if code != 0 {
            bail!(
                "飞书拒绝换 token：{} {}（redirect_uri 必须在应用控制台精确登记）",
                code,
                body.get("error_description")
                    .or_else(|| body.get("msg"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
            );
        }
    }
    let access_token = body
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("飞书应答里没有 access_token：{body}"))?
        .to_string();
    let refresh_token = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(7200);
    Ok(UserToken {
        access_token,
        refresh_token,
        expires_at: now() + expires_in,
    })
}

/// Wait for `GET /callback?code=…&state=…`, answer the browser with a small page.
async fn wait_for_code(listener: TcpListener, expected_state: &str) -> Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await.context("回调端口接不上")?;
        let mut buffer = vec![0u8; 8192];
        let n = stream.read(&mut buffer).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buffer[..n]).to_string();
        let Some(target) = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
        else {
            continue;
        };
        let Some(query) = target.split_once('?').map(|(_, query)| query) else {
            continue;
        };
        let params = parse_query(query);
        let (code, state) = (params.0, params.1);
        let page = if state.as_deref() == Some(expected_state) && code.is_some() {
            "授权完成，可以关掉这个页面回到终端。"
        } else {
            "授权没完成（state 不对或没有 code）—— 回终端重试。"
        };
        let body = format!("<!doctype html><meta charset=utf-8><p>{page}</p>");
        let reply = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(reply.as_bytes()).await;
        if let (Some(code), Some(state)) = (code, state) {
            if state == expected_state {
                return Ok(code);
            }
        }
    }
}

fn parse_query(query: &str) -> (Option<String>, Option<String>) {
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode(value);
        match key {
            "code" => code = Some(value),
            "state" => state = Some(value),
            _ => {}
        }
    }
    (code, state)
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Percent-encode what a query value must escape (app ids and URLs are ASCII).
fn percent(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn random_state() -> String {
    use std::hash::{BuildHasher, Hasher};
    // No RNG dependency for a nonce that only needs to be unpredictable-ish within one
    // browser round trip; the code itself is single-use and bound to the app secret.
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_i64(now());
    hasher.write_u32(std::process::id());
    format!("{:016x}", hasher.finish())
}

fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "linux")]
    let mut command = std::process::Command::new("xdg-open");
    #[cfg(target_os = "windows")]
    let mut command = std::process::Command::new("cmd");
    #[cfg(target_os = "windows")]
    command.args(["/C", "start", ""]);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = url;
        return Ok(());
    }
    command.arg(url).status().context("打不开浏览器")?;
    Ok(())
}

/// Timestamp for the CLI's "logged in until …" line.
pub fn expiry_text(expires_at: i64) -> String {
    chrono::DateTime::from_timestamp(expires_at, 0)
        .map(|time| time.with_timezone(&Utc).to_rfc3339())
        .unwrap_or_else(|| expires_at.to_string())
}
