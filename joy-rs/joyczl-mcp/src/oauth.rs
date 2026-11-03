//! OAuth for MCP remote servers — the browser authorization flow, and where tokens live.
//!
//! The standard browser authorization flow per the MCP authorization spec:
//!
//! ```text
//! 1. discover   GET /.well-known/oauth-protected-resource  → authorization_servers[0]
//!              GET /.well-known/oauth-authorization-server → the endpoints
//! 2. register   POST registration_endpoint (dynamic client registration, DCR)
//! 3. authorize  open the browser → /authorize?...&code_challenge=<PKCE S256>
//!              a local 127.0.0.1:41765/callback catches the redirect
//! 4. exchange   POST token_endpoint (authorization_code + code_verifier)
//! 5. persist    <home>/mcp-auth/<server>.json, 0600, write a temp file then rename
//! ```
//!
//! No secret ever passes through a clipboard or a config file — that is why this second path
//! beside auth_env exists. The port is fixed, not random: the redirect URI registered at DCR
//! must be the same next run; a random port would mean re-registering every time.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// URL codec shared with joyczl-tools' search_web —
// the authorize URL's escaping and the search redirect are the same rule; no two copies.
use joyczl_tools::web::{urldecode, urlencode};

/// Fixed callback port and path (why: see the module comment).
pub const CALLBACK_PORT: u16 = 41765;
pub const CALLBACK_PATH: &str = "/callback";
/// Time a human needs to read the authorization page: minutes, not seconds.
pub const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);
pub const AUTH_DIR: &str = "mcp-auth";

/// A server name becomes a filename — user-provided, not clean.
pub fn sanitise(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

/// One server's credential file. One file per server: two registrations are unrelated, and
/// one corrupt file costs one connection, not all of them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StoredAuth {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<StoredTokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_info: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds. Missing = never expires — the server itself will remind us with a 401.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

pub struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    pub fn new(home: &Path, server_name: &str) -> Self {
        Self {
            path: home
                .join(AUTH_DIR)
                .join(format!("{}.json", sanitise(server_name))),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read. Truncated or hand-mangled files all count as "absent": the cost is one more
    /// sign-in, not a harness that refuses to start.
    pub fn load(&self) -> StoredAuth {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// Write. Temp file first, chmod 0600, then rename — being killed mid-way leaves no
    /// half-good file in place, and the secret is never briefly world-readable.
    pub fn save(&self, auth: &StoredAuth) -> Result<(), String> {
        use std::io::Write;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let raw = serde_json::to_string_pretty(auth).map_err(|e| e.to_string())?;
        let tmp = self.path.with_extension("json.tmp");
        {
            let mut file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
            file.write_all(raw.as_bytes()).map_err(|e| e.to_string())?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())
    }

    pub fn tokens(&self) -> Option<StoredTokens> {
        self.load().tokens
    }

    pub fn save_tokens(&self, tokens: StoredTokens) -> Result<(), String> {
        let mut auth = self.load();
        auth.tokens = Some(tokens);
        self.save(&auth)
    }

    pub fn save_client_info(&self, client_info: serde_json::Value) -> Result<(), String> {
        let mut auth = self.load();
        auth.client_info = Some(client_info);
        self.save(&auth)
    }
}

impl StoredTokens {
    pub fn expired(&self) -> bool {
        match self.expires_at {
            Some(at) => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                // Expire 60 seconds early: don't step over the line while the request flies.
                now >= at - 60
            }
            None => false,
        }
    }
}

// ---- PKCE ------------------------------------------------------------------

/// base64url, no padding. Alphabet per RFC 4648 §5.
pub fn base64url_nopad(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk.first().copied().unwrap_or(0),
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(TABLE[n as usize & 63] as char);
        }
    }
    out
}

/// The PKCE verifier: 43-128 unreserved characters. Uses the system entropy source
/// (/dev/urandom); if unavailable, mixes time + address — weaker, but better than a fixed
/// value, and only reachable on such platforms.
pub fn pkce_verifier() -> String {
    let mut bytes = [0u8; 48];
    let filled = std::fs::File::open("/dev/urandom").and_then(|mut f| {
        use std::io::Read;
        f.read_exact(&mut bytes)
    });
    if filled.is_err() {
        // Fallback entropy: time in nanoseconds + stack address, stirred.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let addr = &bytes as *const _ as u64;
        let mut state = (nanos as u64) ^ addr ^ 0x9E37_79B9_7F4A_7C15;
        for b in bytes.iter_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *b = state as u8;
        }
    }
    base64url_nopad(&bytes)
}

pub fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64url_nopad(&digest)
}

// ---- Endpoint discovery and sign-in ----------------------------------------

#[derive(Debug, Clone)]
pub struct AuthEndpoints {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
}

fn origin(url: &str) -> String {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let authority = rest.split(['/']).next().unwrap_or(rest);
    let prefix = if url.starts_with("https://") {
        "https://"
    } else {
        "http://"
    };
    format!("{prefix}{authority}")
}

/// Discover authorization endpoints from the server URL. Ask the resource metadata first
/// (the MCP-spec way); if it fails, treat the server itself as the authorization server.
pub async fn discover(client: &reqwest::Client, server_url: &str) -> Result<AuthEndpoints, String> {
    // Resource metadata: first the spec's pathless form, then the path-carrying variant.
    let candidates = [
        format!(
            "{}{}/.well-known/oauth-protected-resource",
            origin(server_url),
            path_of(server_url)
        ),
        format!(
            "{}/.well-known/oauth-protected-resource",
            origin(server_url)
        ),
    ];
    let mut auth_server = origin(server_url);
    for candidate in &candidates {
        if let Ok(response) = client.get(candidate).send().await {
            if response.status().is_success() {
                if let Ok(meta) = response.json::<serde_json::Value>().await {
                    if let Some(servers) = meta
                        .get("authorization_servers")
                        .and_then(serde_json::Value::as_array)
                    {
                        if let Some(first) = servers.first().and_then(serde_json::Value::as_str) {
                            auth_server = first.to_string();
                        }
                    }
                }
                break;
            }
        }
    }

    let meta_url = if path_of(&auth_server).is_empty() {
        format!(
            "{}/.well-known/oauth-authorization-server",
            origin(&auth_server)
        )
    } else {
        format!(
            "{}/.well-known/oauth-authorization-server{}",
            origin(&auth_server),
            path_of(&auth_server)
        )
    };
    let meta: serde_json::Value = client
        .get(&meta_url)
        .send()
        .await
        .map_err(|e| format!("授权服务器元数据读不到（{meta_url}）：{e}"))?
        .error_for_status()
        .map_err(|e| format!("授权服务器元数据（{meta_url}）：{e}"))?
        .json()
        .await
        .map_err(|e| format!("授权服务器元数据不是 JSON：{e}"))?;
    let endpoint = |key: &str| {
        meta.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    Ok(AuthEndpoints {
        authorization_endpoint: endpoint("authorization_endpoint")
            .ok_or("元数据里没有 authorization_endpoint")?,
        token_endpoint: endpoint("token_endpoint").ok_or("元数据里没有 token_endpoint")?,
        registration_endpoint: endpoint("registration_endpoint"),
    })
}

fn path_of(url: &str) -> String {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    match rest.find('/') {
        Some(at) => rest[at..].to_string(),
        None => String::new(),
    }
}

/// Dynamic client registration (DCR). client_info is stored; next sign-in skips re-registering.
pub async fn register_client(
    client: &reqwest::Client,
    registration_endpoint: &str,
    redirect_uri: &str,
) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({
        "client_name": "Joy",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    client
        .post(registration_endpoint)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("动态注册发不出去：{e}"))?
        .error_for_status()
        .map_err(|e| format!("动态注册被拒：{e}"))?
        .json()
        .await
        .map_err(|e| format!("注册应答不是 JSON：{e}"))
}

/// Local callback: listen once, catch the redirect, return the query params.
/// `open_browser` is injected — tests never open a real browser.
async fn catch_callback(
    open_browser: &dyn Fn(&str),
    authorize_url: &str,
) -> Result<Vec<(String, String)>, String> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .await
        .map_err(|e| {
            format!("端口 {CALLBACK_PORT} 被占着 —— 登录重定向没地方落。关掉占着它的进程再试：{e}")
        })?;
    open_browser(authorize_url);

    let timeout = tokio::time::sleep(SIGN_IN_TIMEOUT);
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            _ = &mut timeout => {
                return Err("等浏览器登录回来超时了".to_string());
            }
            accepted = listener.accept() => {
                let (mut socket, _) = accepted.map_err(|e| format!("回调连接收不到：{e}"))?;
                // Read only until the HTTP headers end — the browser holds the connection
                // open, and read_to_string would wait until the end of time.
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut request = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    let header_end = find_header_end(&request);
                    if header_end.is_some() {
                        break;
                    }
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&chunk[..n]),
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let request_line = request.lines().next().unwrap_or_default();
                let target = request_line.split_whitespace().nth(1).unwrap_or_default();
                if !target.starts_with(CALLBACK_PATH) {
                    let _ = socket
                        .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n")
                        .await;
                    continue;
                }
                let query = target.split('?').nth(1).unwrap_or_default();
                let params: Vec<(String, String)> = query
                    .split('&')
                    .filter(|p| !p.is_empty())
                    .map(|pair| match pair.split_once('=') {
                        Some((k, v)) => (k.to_string(), urldecode(v)),
                        None => (pair.to_string(), String::new()),
                    })
                    .collect();
                let page = "<html><body style='font-family:system-ui;padding:3rem'>\
        <h2>Signed in.</h2><p>You can close this tab and return to the terminal.</p>\
        </body></html>";
                let _ = socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\n\
                             content-length: {}\r\nconnection: close\r\n\r\n{page}",
                            page.len()
                        )
                        .as_bytes(),
                    )
                    .await;
                return Ok(params);
            }
        }
    }
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|w| w == b"\r\n\r\n")
}

/// One full sign-in: discover → (register) → authorize → exchange → persist.
/// Returns the access_token; the caller uses it to connect.
pub async fn sign_in(
    client: &reqwest::Client,
    home: &Path,
    server_name: &str,
    server_url: &str,
    open_browser: &dyn Fn(&str),
) -> Result<String, String> {
    let store = TokenStore::new(home, server_name);
    let endpoints = discover(client, server_url).await?;
    let redirect_uri = format!("http://127.0.0.1:{CALLBACK_PORT}{CALLBACK_PATH}");

    // Client info: use the registered one; if absent and the server supports DCR, register now.
    let client_info = match store.load().client_info {
        Some(info) => info,
        None => {
            let endpoint = endpoints
                .registration_endpoint
                .as_deref()
                .ok_or("服务器不支持动态注册，也没有现成的 client_id —— 手工注册后把 client_id 写进 mcp-auth 的 client_info")?;
            let info = register_client(client, endpoint, &redirect_uri).await?;
            store.save_client_info(info.clone())?;
            info
        }
    };
    let client_id = client_info
        .get("client_id")
        .and_then(serde_json::Value::as_str)
        .ok_or("注册应答里没有 client_id")?
        .to_string();

    let verifier = pkce_verifier();
    let challenge = pkce_challenge(&verifier);
    let state = pkce_verifier();
    let authorize_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
        endpoints.authorization_endpoint,
        urlencode(&client_id),
        urlencode(&redirect_uri),
        urlencode(&state),
        challenge,
    );

    let params = catch_callback(open_browser, &authorize_url).await?;
    let get = |key: &str| {
        params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    if let Some(error) = get("error") {
        // The server said no. Use its own words: access_denied and invalid_client lead to
        // completely different places.
        let detail = get("error_description")
            .map(|d| format!(" — {d}"))
            .unwrap_or_default();
        return Err(format!("授权失败：{error}{detail}"));
    }
    let code = get("code").ok_or("回调里没有 code")?;
    if let Some(returned_state) = get("state") {
        if returned_state != state {
            return Err("state 对不上 —— 可能是伪造的回调，拒绝换 token".to_string());
        }
    }

    let token: serde_json::Value = client
        .post(&endpoints.token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", client_id.as_str()),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("换 token 发不出去：{e}"))?
        .error_for_status()
        .map_err(|e| format!("换 token 被拒：{e}"))?
        .json()
        .await
        .map_err(|e| format!("token 应答不是 JSON：{e}"))?;
    let tokens = tokens_from_response(&token)?;
    store.save_tokens(tokens.clone())?;
    Ok(tokens.access_token)
}

fn tokens_from_response(token: &serde_json::Value) -> Result<StoredTokens, String> {
    let access = token
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .ok_or("token 应答里没有 access_token")?;
    let expires_at = token
        .get("expires_in")
        .and_then(serde_json::Value::as_i64)
        .map(|secs| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64 + secs)
                .unwrap_or(0)
        });
    Ok(StoredTokens {
        access_token: access.to_string(),
        refresh_token: token
            .get("refresh_token")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        expires_at,
    })
}

/// Get the token before connecting: use it if present and unexpired; refresh if expired with
/// a refresh_token; otherwise report "go sign in". A successful refresh persists the new token.
pub async fn ensure_access_token(
    home: &Path,
    server_name: &str,
    server_url: &str,
) -> Result<String, String> {
    let store = TokenStore::new(home, server_name);
    let tokens = store.tokens().ok_or_else(|| {
        format!("还没登录过。跑 joy mcp login {server_name} 完成一次浏览器授权。")
    })?;
    if !tokens.expired() {
        return Ok(tokens.access_token);
    }
    let refresh = tokens.refresh_token.clone().ok_or_else(|| {
        format!("token 过期了且没有 refresh_token。重跑 joy mcp login {server_name}。")
    })?;

    let client = reqwest::Client::new();
    let endpoints = discover(&client, server_url).await?;
    let client_id = store
        .load()
        .client_info
        .and_then(|info| {
            info.get("client_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .ok_or("没有 client_info —— 重跑 joy mcp login")?;
    let token: serde_json::Value = client
        .post(&endpoints.token_endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.as_str()),
            ("client_id", client_id.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("刷新发不出去：{e}"))?
        .error_for_status()
        .map_err(|e| format!("刷新被拒：{e}"))?
        .json()
        .await
        .map_err(|e| format!("刷新应答不是 JSON：{e}"))?;
    let mut fresh = tokens_from_response(&token)?;
    if fresh.refresh_token.is_none() {
        fresh.refresh_token = Some(refresh);
    }
    store.save_tokens(fresh.clone())?;
    Ok(fresh.access_token)
}
