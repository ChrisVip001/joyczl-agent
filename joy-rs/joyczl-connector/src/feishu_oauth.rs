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
//!   5. **and again in the background**, ten minutes before each expiry, so the chain never
//!      dies from neglect and a send never pays the refresh latency
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
/// Refresh this long before expiry. Long enough to survive a sleeping laptop, short enough
/// that a revoked authorization shows up while the human still remembers logging in.
pub const REFRESH_LEAD: i64 = 600;
/// Longest the background loop sleeps between glances at the token file (so a login that
/// happens *after* the server booted is picked up too).
const MAX_SLEEP: Duration = Duration::from_secs(300);
/// Shortest sleep after a refresh, so a provider answering with a tiny `expires_in` cannot
/// turn the loop into a hot spin.
const MIN_SLEEP: Duration = Duration::from_secs(30);
/// How long to wait after a *transient* failure (network, 500) before trying again.
const RETRY_BACKOFF: Duration = Duration::from_secs(300);
/// A lock file older than this is from a process that died mid-refresh; take it over.
const LOCK_STALE: Duration = Duration::from_secs(60);

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
    std::fs::create_dir_all(path.parent().expect("it has a parent"))?;
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
            "Not signed in to Feishu as a user yet: run `joy connector feishu login` first\n\n             (or set feishu.as back to the default app)"
        );
    };
    if token.expires_at - 300 > now() {
        return Ok(token.access_token);
    }
    Ok(refresh_now(config, home).await?.access_token)
}

/// Refresh the stored token, once, whoever asks — the lazy send path and the background
/// loop share this.
///
/// A refresh is a **rotation**: the old refresh token dies when the new one is issued, so
/// two refreshers at the same moment would lose the chain. Inside the process a file lock
/// serializes them (`connector-auth/feishu.lock`, stale after a minute); after taking it the
/// token is re-read, which is also how "another process already did it" becomes a no-op
/// instead of a second rotation.
pub async fn refresh_now(config: &FeishuConfig, home: &Path) -> Result<UserToken> {
    let _lock = lock(home).await?;
    let Some(token) = load(home) else {
        bail!(
            "no Feishu user token file ({} is not there)",
            token_path(home).display()
        );
    };
    if token.expires_at - REFRESH_LEAD > now() {
        // Someone refreshed while we waited for the lock: their token is the good one.
        return Ok(token);
    }
    let Some(secret) = config.secret(home) else {
        bail!(
            "the Feishu app_secret could not be found, so the user token cannot be refreshed ({})",
            config.app_secret
        );
    };
    let refreshed = exchange(
        config,
        &json!({
            "grant_type": "refresh_token",
            "client_id": config.app_id,
            "client_secret": secret,
            "refresh_token": token.refresh_token,
        }),
    )
    .await
    .context("refreshing the Feishu user token failed; run `joy connector feishu login` again")?;
    save(home, &refreshed)?;
    Ok(refreshed)
}

/// How long the background loop should sleep before looking again.
///
/// `ZERO` means "refresh now"; `MIN_SLEEP` keeps a provider that hands out very short tokens
/// from spinning the loop, and `MAX_SLEEP` keeps it able to notice a fresh login.
pub fn next_check(expires_at: i64, now: i64) -> Duration {
    let until = expires_at - REFRESH_LEAD - now;
    if until <= 0 {
        return Duration::ZERO;
    }
    Duration::from_secs((until as u64).min(MAX_SLEEP.as_secs()))
}

/// Start the background renewer. Meant for the long-lived process (app-server); the CLI
/// keeps its lazy refresh.
///
/// It keeps running while there is no token file at all: a human may run `feishu login`
/// after the server booted, and the token should then be looked after without a restart.
pub fn spawn_renewer(config: FeishuConfig, home: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move { renew_forever(config, home).await })
}

async fn renew_forever(config: FeishuConfig, home: PathBuf) {
    loop {
        let wait = match load(&home) {
            None => MAX_SLEEP,
            Some(token) => next_check(token.expires_at, now()),
        };
        if wait > Duration::ZERO {
            tokio::time::sleep(wait).await;
            continue;
        }
        match refresh_now(&config, &home).await {
            Ok(token) => {
                eprintln!(
                    "(joy) the Feishu user token was refreshed in the background; it expires at {}",
                    expiry_text(token.expires_at)
                );
                // A successful refresh usually leaves hours on the clock; the floor only
                // matters when the provider answered with something tiny.
                tokio::time::sleep(MIN_SLEEP).await;
            }
            Err(error) if is_terminal(&error) => {
                // The refresh token itself is dead (revoked, expired, wrong app). No amount of
                // retrying fixes that, and silence would leave sends failing with no clue.
                eprintln!(
                    "(joy) the Feishu user token could not be refreshed and needs a fresh sign-in: joy connector feishu login\n      {error:#}"
                );
                return;
            }
            Err(error) => {
                eprintln!("(joy) the Feishu user token was not refreshed this time; trying again in 5 minutes: {error:#}");
                tokio::time::sleep(RETRY_BACKOFF).await;
            }
        }
    }
}

/// Did Feishu itself refuse the grant (as opposed to "the network was down")?
///
/// Only a refusal is terminal: retrying a revoked authorization every five minutes would
/// just write the same line into a log forever.
fn is_terminal(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<Rejected>().is_some())
}

/// Feishu answered and said no. Marked as its own type so the renewer can tell it apart
/// from a timeout without matching on text.
#[derive(Debug)]
struct Rejected(String);

impl std::fmt::Display for Rejected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Rejected {}

/// An exclusive `connector-auth/feishu.lock`; removed on drop.
///
/// Hand-rolled instead of pulling in a lock crate: one file, one meaning. A crash leaves the
/// file behind, so it is considered free again after `LOCK_STALE`.
struct Lock(PathBuf);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn lock(home: &Path) -> Result<Lock> {
    let path = home.join(AUTH_DIR).join("feishu.lock");
    std::fs::create_dir_all(path.parent().expect("it has a parent"))?;
    for _ in 0..50 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write as _;
                let _ = write!(file, "{} {}", std::process::id(), now());
                return Ok(Lock(path));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&path)
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|at| at.elapsed().ok())
                    .is_some_and(|age| age > LOCK_STALE);
                if stale {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
    bail!(
        "another process is refreshing the Feishu token ({}); skipping this time",
        path.display()
    )
}

/// The human's login: open the browser, catch the code, exchange it, store it.
/// Returns the token's expiry for the message.
pub async fn login(config: &FeishuConfig, home: &Path) -> Result<i64> {
    let Some(secret) = config.secret(home) else {
        bail!(
            "the Feishu app_secret could not be found ({}), so the token cannot be exchanged",
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
        .with_context(|| {
            format!("port {CALLBACK_PORT} is taken (perhaps the last sign-in is still open)")
        })?;
    eprintln!("(joy) finish the authorisation in the browser. If it does not open, paste this address:\n{url}");
    let _ = open_browser(&url);

    let code = tokio::time::timeout(SIGN_IN_TIMEOUT, wait_for_code(listener, &state))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "waited {}s for the authorisation and gave up",
                SIGN_IN_TIMEOUT.as_secs()
            )
        })??;

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
        .context("could not reach the Feishu authorisation endpoint")?;
    let body: Value = response
        .json()
        .await
        .context("the Feishu authorisation endpoint did not return JSON")?;
    // v2 answers 0 on success; some deployments answer without a code at all.
    if let Some(code) = body.get("code").and_then(Value::as_i64) {
        if code != 0 {
            // A refusal, not a hiccup: the background renewer stops on this and asks for a
            // human instead of retrying into the void.
            return Err(anyhow::Error::new(Rejected(format!(
                "Feishu refused to exchange the token: {} {} (redirect_uri must be registered exactly in the app console)",
                code,
                body.get("error_description")
                    .or_else(|| body.get("msg"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
            ))));
        }
    }
    let access_token = body
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("the Feishu reply has no access_token: {body}"))?
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
        let (mut stream, _) = listener
            .accept()
            .await
            .context("the callback port would not accept a connection")?;
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
            "Authorised — you can close this page and go back to the terminal."
        } else {
            "Authorisation did not complete (wrong state, or no code) — retry from the terminal."
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
    command
        .arg(url)
        .status()
        .context("could not open the browser")?;
    Ok(())
}

/// Timestamp for the CLI's "logged in until …" line.
pub fn expiry_text(expires_at: i64) -> String {
    chrono::DateTime::from_timestamp(expires_at, 0)
        .map(|time| time.with_timezone(&Utc).to_rfc3339())
        .unwrap_or_else(|| expires_at.to_string())
}
