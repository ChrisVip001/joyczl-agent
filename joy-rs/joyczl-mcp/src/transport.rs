//! Two transports, one request interface.
//!
//! stdio: the client spawns the server as a local subprocess, one JSON-RPC message per line.
//! Streamable HTTP: the server is already running elsewhere; POST to it.
//!
//! Both paths do one thing: turn `request(method, params)` into `result`.
//! The rest (who connects, what tools are called) lives above.
//!
//! A session is **exclusive**: a request goes out and we read until the matching answer.
//! So every connection is wrapped in a Mutex (see `lib.rs`) — concurrent calls queue
//! instead of eating each other's answers.

use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// MCP protocol version. The server may answer with its own supported version; if not it
/// still answers — we only use the `tools` capability, which version differences don't touch.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const CLIENT_NAME: &str = "joy";

/// Per-request cap. Tool calls included — a stuck server must not stall a whole turn.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// One tool reported by the server.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolMeta {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// The wire field is `inputSchema`; the SDK 2.x Python port calls it `input_schema`.
    /// Accept both spellings, so a single underscore cannot break the connection.
    #[serde(default, rename = "inputSchema", alias = "input_schema")]
    pub input_schema: Option<Value>,
}

impl ToolMeta {
    /// Schema for the model: if the server gave none, degrade to "an object with no parameters".
    pub fn schema(&self) -> Value {
        match &self.input_schema {
            Some(Value::Object(_)) => self.input_schema.clone().unwrap(),
            _ => json!({"type": "object", "properties": {}}),
        }
    }
}

/// A session speaking one JSON-RPC message per line.
///
/// The generics exist so tests can use `tokio::io::duplex` as the pipe — "is the protocol
/// right" can then be tested without spawning a real subprocess.
pub struct StdioSession<R, W> {
    reader: BufReader<R>,
    writer: W,
    next_id: u64,
}

impl<R, W> StdioSession<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    pub fn new(reader: R, writer: W) -> Self {
        StdioSession {
            reader: BufReader::new(reader),
            writer,
            next_id: 1,
        }
    }

    async fn send(&mut self, message: &Value) -> Result<(), String> {
        let mut line = serde_json::to_string(message).map_err(|e| e.to_string())?;
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("写不出去：{e}"))?;
        self.writer
            .flush()
            .await
            .map_err(|e| format!("写不出去：{e}"))
    }

    pub async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.send(&message).await
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send(&message).await?;

        loop {
            let mut line = String::new();
            let read = self
                .reader
                .read_line(&mut line)
                .await
                .map_err(|e| format!("读不到：{e}"))?;
            if read == 0 {
                return Err("服务器把连接关了".to_string());
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(message) = serde_json::from_str::<Value>(trimmed) else {
                // The server printed a non-JSON line to stdout (some libraries do that).
                // Skip it rather than condemning the whole connection.
                continue;
            };
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                // Before our answer there may be other messages: notifications, or requests
                // initiated by the server itself (elicitation / sampling).
                //
                // Reverse requests **cannot be ignored**: the server would wait forever for an
                // answer that never comes (some hang right there). An explicit "unsupported"
                // at least says this side won't answer. Full support not done — see docs/limitations.md.
                if let (Some(method), Some(their_id)) = (
                    message.get("method").and_then(Value::as_str),
                    message.get("id").cloned(),
                ) {
                    eprintln!("(joy) MCP 服务器发来反向请求 '{method}'，已明确回绝（不支持）");
                    let reply = json!({
                        "jsonrpc": "2.0",
                        "id": their_id,
                        "error": {
                            "code": -32601,
                            "message": format!("Joy 不支持反向请求 '{method}'"),
                        },
                    });
                    self.send(&reply).await?;
                }
                continue;
            }
            return unwrap_message(&message);
        }
    }
}

/// Handshake, list tools and call tools are identical across both transports, so they live
/// on the trait; each framing only handles "how to get one message out and back".
///
/// Method names carry the `rpc_` prefix to separate them from the two structs' own
/// `request` / `notify` — those target concrete types; no same-named parsing rules.
///
/// The `async_fn_in_trait` lint targets "handing a trait to others to implement"; here
/// only the two implementations in this file exist and it is never used as `dyn`, so the
/// Future needs no `Send` bound.
#[allow(async_fn_in_trait)]
pub trait Rpc {
    async fn rpc_request(&mut self, method: &str, params: Value) -> Result<Value, String>;
    async fn rpc_notify(&mut self, method: &str, params: Value) -> Result<(), String>;
}

impl<R, W> Rpc for StdioSession<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    async fn rpc_request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        StdioSession::request(self, method, params).await
    }

    async fn rpc_notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        StdioSession::notify(self, method, params).await
    }
}

impl Rpc for HttpSession {
    async fn rpc_request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        HttpSession::request(self, method, params).await
    }

    async fn rpc_notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        HttpSession::notify(self, method, params).await
    }
}

/// Handshake: announce who we are, get the server's capabilities, then say "we're ready".
pub async fn initialize(session: &mut impl Rpc) -> Result<Value, String> {
    let result = session
        .rpc_request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": CLIENT_NAME, "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .await?;
    session
        .rpc_notify("notifications/initialized", json!({}))
        .await?;
    Ok(result)
}

pub async fn list_tools(session: &mut impl Rpc) -> Result<Vec<ToolMeta>, String> {
    let result = session.rpc_request("tools/list", json!({})).await?;
    let tools = result.get("tools").cloned().unwrap_or(Value::Array(vec![]));
    serde_json::from_value(tools).map_err(|e| format!("工具列表读不懂：{e}"))
}

/// Call a tool once, returning the text for the model to read.
pub async fn call_tool(session: &mut impl Rpc, tool: &str, args: Value) -> Result<String, String> {
    let result = session
        .rpc_request("tools/call", json!({"name": tool, "arguments": args}))
        .await?;
    let mut parts: Vec<String> = Vec::new();
    for block in result
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let text = block
            .get("text")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .unwrap_or("[非文本内容]");
        parts.push(text.to_string());
    }
    let joined = parts.join("\n");
    let joined = if joined.is_empty() {
        "(没有输出)".to_string()
    } else {
        joined
    };
    if result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(joined);
    }
    Ok(joined)
}

impl<R, W> StdioSession<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    pub async fn initialize(&mut self) -> Result<Value, String> {
        initialize(self).await
    }

    pub async fn list_tools(&mut self) -> Result<Vec<ToolMeta>, String> {
        list_tools(self).await
    }

    pub async fn call_tool(&mut self, tool: &str, args: Value) -> Result<String, String> {
        call_tool(self, tool, args).await
    }
}

/// Take `result` out of a JSON-RPC message, or turn `error` into an error.
fn unwrap_message(message: &Value) -> Result<Value, String> {
    if let Some(error) = message.get("error") {
        if !error.is_null() {
            let text = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("（没有说明）");
            let code = error.get("code").map(Value::to_string).unwrap_or_default();
            return Err(format!("{text} ({code})"));
        }
    }
    Ok(message.get("result").cloned().unwrap_or(Value::Null))
}

/// Streamable HTTP session. The old HTTP+SSE transport is deprecated; unsupported on purpose.
pub struct HttpSession {
    client: reqwest::Client,
    url: String,
    token: Option<String>,
    /// Given by the server in the initialize answer; every later request must carry it back.
    session_id: Option<String>,
    next_id: u64,
}

impl HttpSession {
    pub fn new(url: &str, token: Option<String>) -> Self {
        HttpSession {
            client: reqwest::Client::new(),
            url: url.to_string(),
            token,
            session_id: None,
            next_id: 1,
        }
    }

    async fn post(&mut self, body: Value) -> Result<(u16, String, String), String> {
        let mut request = self
            .client
            .post(&self.url)
            .header("content-type", "application/json")
            // Announce both: the server may answer as JSON or open an SSE stream.
            .header("accept", "application/json, text/event-stream")
            .json(&body);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        if let Some(session) = &self.session_id {
            request = request.header("mcp-session-id", session);
        }
        let response = request
            .send()
            .await
            .map_err(|e| format!("请求发不出去：{e}"))?;
        let status = response.status().as_u16();
        if let Some(session) = response.headers().get("mcp-session-id") {
            if let Ok(session) = session.to_str() {
                self.session_id = Some(session.to_string());
            }
        }
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let text = response
            .text()
            .await
            .map_err(|e| format!("读不到回话：{e}"))?;
        Ok((status, content_type, text))
    }

    pub async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        let (status, _, text) = self
            .post(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await?;
        if status == 202 || text.trim().is_empty() {
            return Ok(()); // a notification's answer is just "received"
        }
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}：{}", first_line(&text)));
        }
        Ok(())
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let (status, content_type, text) = self
            .post(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        if !(200..300).contains(&status) {
            // Say the status code out loud: generic SDKs often blur 401 and 500 into one
            // "the server returned an error", when that is exactly what people want to know.
            return Err(format!("HTTP {status}：{}", first_line(&text)));
        }
        if content_type.contains("text/event-stream") {
            let message = find_in_stream(&text, id)
                .ok_or_else(|| "SSE 流里没有我们要的那个应答".to_string())?;
            return unwrap_message(&message);
        }
        if text.trim().is_empty() {
            return Err("服务器回了个空身体".to_string());
        }
        let message: Value =
            serde_json::from_str(&text).map_err(|e| format!("回话不是 JSON：{e}"))?;
        match message {
            Value::Array(messages) => messages
                .iter()
                .find(|m| m.get("id").and_then(Value::as_u64) == Some(id))
                .map(unwrap_message)
                .unwrap_or_else(|| Err("回话里没有我们要的那个应答".to_string())),
            other => unwrap_message(&other),
        }
    }

    pub async fn initialize(&mut self) -> Result<Value, String> {
        initialize(self).await
    }

    pub async fn list_tools(&mut self) -> Result<Vec<ToolMeta>, String> {
        list_tools(self).await
    }

    pub async fn call_tool(&mut self, tool: &str, args: Value) -> Result<String, String> {
        call_tool(self, tool, args).await
    }
}

/// Find the message matching `id` inside an SSE stream.
fn find_in_stream(text: &str, id: u64) -> Option<Value> {
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(message) = serde_json::from_str::<Value>(data.trim()) else {
            continue;
        };
        if message.get("id").and_then(Value::as_u64) == Some(id) {
            return Some(message);
        }
    }
    None
}

/// On server errors the body can be a whole page of HTML. Only the first line fits in a log.
fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    line.chars().take(200).collect()
}

/// A transport that is already connected.
pub enum Transport {
    Stdio(Box<StdioTransport>),
    Http(HttpSession),
}

/// The two handles of the stdio path: the pipe session + the subprocess itself.
///
/// Boxed — it is an order of magnitude larger than `HttpSession`; left in the enum variant
/// every `Transport` would occupy the largest variant's size.
pub struct StdioTransport {
    session: StdioSession<ChildStdout, ChildStdin>,
    /// The subprocess handle, only to keep it alive (kill_on_drop takes it away on drop).
    _child: Child,
}

impl Transport {
    /// Start a transport from config. **Not yet** handshaken — that is [`Transport::initialize`].
    pub async fn connect(spec: &crate::config::Transport) -> Result<Transport, String> {
        match spec {
            crate::config::Transport::Stdio { command, args, env } => {
                let mut child = Command::new(command);
                child
                    .args(args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    // The server's logs go to our stderr: stdout is the protocol, stderr is logs.
                    .stderr(Stdio::inherit())
                    .kill_on_drop(true);
                if let Some(env) = env {
                    // Setting env means **replace**, not append.
                    child.env_clear();
                    child.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
                }
                let mut child = child
                    .spawn()
                    .map_err(|e| format!("起不来 '{command}'：{e}"))?;
                let stdin = child.stdin.take().ok_or("拿不到子进程的 stdin")?;
                let stdout = child.stdout.take().ok_or("拿不到子进程的 stdout")?;
                Ok(Transport::Stdio(Box::new(StdioTransport {
                    session: StdioSession::new(stdout, stdin),
                    _child: child,
                })))
            }
            crate::config::Transport::Http { url, token } => {
                Ok(Transport::Http(HttpSession::new(url, token.clone())))
            }
        }
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        match self {
            Transport::Stdio(stdio) => stdio.session.request(method, params).await,
            Transport::Http(session) => session.request(method, params).await,
        }
    }

    pub async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        match self {
            Transport::Stdio(stdio) => stdio.session.notify(method, params).await,
            Transport::Http(session) => session.notify(method, params).await,
        }
    }

    pub async fn initialize(&mut self) -> Result<Value, String> {
        match self {
            Transport::Stdio(stdio) => stdio.session.initialize().await,
            Transport::Http(session) => session.initialize().await,
        }
    }

    pub async fn list_tools(&mut self) -> Result<Vec<ToolMeta>, String> {
        match self {
            Transport::Stdio(stdio) => stdio.session.list_tools().await,
            Transport::Http(session) => session.list_tools().await,
        }
    }

    /// Call a tool once, returning the text for the model to read.
    pub async fn call_tool(&mut self, tool: &str, args: Value) -> Result<String, String> {
        match self {
            Transport::Stdio(stdio) => stdio.session.call_tool(tool, args).await,
            Transport::Http(session) => session.call_tool(tool, args).await,
        }
    }
}
