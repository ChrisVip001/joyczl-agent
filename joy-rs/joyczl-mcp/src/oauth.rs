//! MCP 远程服务器的 OAuth —— 浏览器授权流，以及 token 住在哪。
//!
//! 按 MCP 的 authorization 规范实现的标准浏览器授权流：
//!
//! ```text
//! 1. 发现     GET /.well-known/oauth-protected-resource  → authorization_servers[0]
//!             GET /.well-known/oauth-authorization-server → 各端点
//! 2. 注册     POST registration_endpoint（动态客户端注册，DCR）
//! 3. 授权     打开浏览器 → /authorize?...&code_challenge=<PKCE S256>
//!             本地 127.0.0.1:41765/callback 接住重定向
//! 4. 换 token POST token_endpoint（authorization_code + code_verifier）
//! 5. 落盘     <home>/mcp-auth/<server>.json，0600，先写临时文件再改名
//! ```
//!
//! 没有秘密经过人的剪贴板或配置文件 —— 这就是 auth_env 之外的另一条路
//! 存在的理由。port 固定而不是随机的：redirect URI 在 DCR 时登记给了
//! 授权服务器，下次跑还得是同一个，随机端口等于每次都重注册。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// URL 编解码与 joyczl-tools 的 search_web 共用同一份实现 ——
// authorize URL 的转义规则和搜索跳转是同一条，不该有两份。
use joyczl_tools::web::{urldecode, urlencode};

/// 回调固定端口与路径（理由见模块注释）。
pub const CALLBACK_PORT: u16 = 41765;
pub const CALLBACK_PATH: &str = "/callback";
/// 人读授权页的时间：是分钟不是秒。
pub const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);
pub const AUTH_DIR: &str = "mcp-auth";

/// 服务器名要变成文件名 —— 用户给的，不干净。
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

/// 一个服务器的凭证文件。一服务器一文件：两家的注册互不相干，一个文件
/// 坏了只损失一条连接，而不是全部。
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
    /// Unix 秒。没有就当永不过期 —— 服务器自己会在 401 里提醒我们。
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

    /// 读。截断的、手改坏的文件都当「没有」：代价是重新登录一次，
    /// 而不是让整个 harness 起不来。
    pub fn load(&self) -> StoredAuth {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// 写。先写临时文件、chmod 0600、再改名 —— 中途被杀不会留下半个
    /// 好文件的位置，秘密也从不短暂地世界可读。
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
                // 提前 60 秒判过期：请求飞过去的路上也别踩线。
                now >= at - 60
            }
            None => false,
        }
    }
}

// ---- PKCE ------------------------------------------------------------------

/// base64url，无填充。 alphabet 表见 RFC 4648 §5。
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

/// PKCE 的 verifier：43-128 个未保留字符。用系统熵源（/dev/urandom）；
/// 拿不到就用时间 + 地址混合 —— 弱一些，但比固定值强，且只在这种
/// 平台上才会走到。
pub fn pkce_verifier() -> String {
    let mut bytes = [0u8; 48];
    let filled = std::fs::File::open("/dev/urandom").and_then(|mut f| {
        use std::io::Read;
        f.read_exact(&mut bytes)
    });
    if filled.is_err() {
        // 兜底熵：时间纳秒 + 栈地址，搅一搅。
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

// ---- 端点发现与登录 --------------------------------------------------------

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

/// 从服务器 URL 一路发现授权端点。先问资源元数据（MCP 规范的走法），
/// 问不到就把服务器自己当授权服务器（很多实现就是这么部署的）。
pub async fn discover(client: &reqwest::Client, server_url: &str) -> Result<AuthEndpoints, String> {
    // 资源元数据：先按规范的无路径名，再按带路径的变体。
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

/// 动态客户端注册（DCR）。client_info 存下来，下次登录不再重注册。
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

/// 本地回调：监听一次、接住重定向、返回查询参数。
/// `open_browser` 是注入的 —— 测试不开真浏览器。
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
                // 只读到 HTTP 头结束为止 —— 浏览器握着连接不放，
                // read_to_string 会等到天荒地老。
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

/// 完整的一次登录：发现 → （注册）→ 授权 → 换 token → 落盘。
/// 返回 access_token，调用方拿它连服务器。
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

    // 客户端信息：注册过的直接用；没有而服务器支持 DCR 就现在注册。
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
        // 服务器说了不。用它的原话：access_denied 和 invalid_client 会把人
        // 带去完全不同的地方。
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

/// 连接前的取 token：有且没过期就用；过期且有 refresh_token 就刷新；
/// 都没有就报「去登录」。刷新成功会把新 token 落盘。
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
