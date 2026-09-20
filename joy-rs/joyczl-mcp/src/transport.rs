//! 两条传输，一个请求接口。
//!
//! stdio：客户端把服务器当本地子进程起起来，一行一个 JSON-RPC 消息。
//! Streamable HTTP：服务器已经在别处跑着，POST 过去。
//!
//! 两条路都只做一件事：把 `request(method, params)` 变成 `result`。
//! 剩下的（谁连得上、工具叫什么）在上层。
//!
//! 会话是**独占**的：一次请求发出去，读到匹配的那个应答为止。所以每条连接
//! 外面套一把 Mutex（见 `lib.rs`）—— 并发调用排队，而不是互相吃掉对方的
//! 应答。

use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// MCP 协议版本。服务器可以回它自己支持的版本；不支持的话也会照常回话，
/// 我们只用 `tools` 这一项能力，版本差异影响不到它。
pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const CLIENT_NAME: &str = "joy";

/// 一次请求的上限。工具调用也在内 —— 一个卡住的服务器不该拖住一整轮对话。
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// 服务器报上来的一个工具。
#[derive(Debug, Clone, Deserialize)]
pub struct ToolMeta {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// 线上字段是 `inputSchema`；SDK 2.x 的 Python 端口叫 `input_schema`，
    /// 两种写法都收，省得因为一个下划线连不上。
    #[serde(default, rename = "inputSchema", alias = "input_schema")]
    pub input_schema: Option<Value>,
}

impl ToolMeta {
    /// 给模型看的 schema：服务器没给就退化成「没有参数的对象」。
    pub fn schema(&self) -> Value {
        match &self.input_schema {
            Some(Value::Object(_)) => self.input_schema.clone().unwrap(),
            _ => json!({"type": "object", "properties": {}}),
        }
    }
}

/// 一行一个 JSON-RPC 消息的会话。
///
/// 泛型是为了测试能拿 `tokio::io::duplex` 当管子用 —— 于是「协议对不对」
/// 这件事不需要起一个真的子进程就能测。
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
                // 服务器往 stdout 打了一行不是 JSON 的东西（某些库会这样）。
                // 跳过它，而不是把整条连接判死。
                continue;
            };
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                // 我们的应答之前可能有别的消息：通知，或者服务器反过来
                // 发起的请求。这个客户端不支持采样/反向提问，跳过。
                continue;
            }
            return unwrap_message(&message);
        }
    }
}

/// 握手、列工具、调工具这三件事两条传输一模一样，所以它们写在 trait 上，
/// 两种帧各自只负责「怎么把一条消息送出去、拿回来」。
///
/// 方法名带 `rpc_` 前缀是为了跟两个结构体自己的 `request` / `notify` 区分开
/// —— 那对方法是照具体类型写的，不想要重名的解析规则来添乱。
///
/// `async_fn_in_trait` 这条 lint 针对的是「把 trait 交给别人实现」，而这里
/// 只有本文件里那两个实现，也从没把它当 `dyn` 用，所以不需要给 Future 补
/// `Send` 约束。
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

/// 握手：报上自己是谁、拿到服务器的能力，然后告诉它「我准备好了」。
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

/// 调一次工具，返回给模型读的文本。
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

/// 从一条 JSON-RPC 消息里取出 `result`，或者把 `error` 变成错误。
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

/// Streamable HTTP 会话。老的 HTTP+SSE 传输已废弃，这里有意不支持。
pub struct HttpSession {
    client: reqwest::Client,
    url: String,
    token: Option<String>,
    /// 服务器在 initialize 的应答里给的；之后每个请求都要带回去。
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
            // 两条都报上：服务器可以按 JSON 回，也可以开一条 SSE 流。
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
            return Ok(()); // 通知的应答就是「收到了」
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
            // 这里多说一句状态码：通用 SDK 常把 401 和 500 都糊成
            // 一句「服务器返回了错误」，而人最想知道的恰恰是哪一个。
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

/// 从一条 SSE 流里找出 `id` 对应的那条消息。
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

/// 服务器报错时，回话可能是一整页 HTML。日志里只配放第一行。
fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    line.chars().take(200).collect()
}

/// 一条已经连上的传输。
pub enum Transport {
    Stdio {
        session: StdioSession<ChildStdout, ChildStdin>,
        /// 子进程的把手，只为了活着（kill_on_drop 让它在被丢掉时一起走）。
        _child: Child,
    },
    Http(HttpSession),
}

impl Transport {
    /// 按配置起一条传输。**还没**握手 —— 握手是 [`Transport::initialize`]。
    pub async fn connect(spec: &crate::config::Transport) -> Result<Transport, String> {
        match spec {
            crate::config::Transport::Stdio { command, args, env } => {
                let mut child = Command::new(command);
                child
                    .args(args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    // 服务器的日志走我们的 stderr：stdout 是协议通道，stderr 是日志。
                    .stderr(Stdio::inherit())
                    .kill_on_drop(true);
                if let Some(env) = env {
                    // 设了 env 就是**替换**，不是追加。
                    child.env_clear();
                    child.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
                }
                let mut child = child
                    .spawn()
                    .map_err(|e| format!("起不来 '{command}'：{e}"))?;
                let stdin = child.stdin.take().ok_or("拿不到子进程的 stdin")?;
                let stdout = child.stdout.take().ok_or("拿不到子进程的 stdout")?;
                Ok(Transport::Stdio {
                    session: StdioSession::new(stdout, stdin),
                    _child: child,
                })
            }
            crate::config::Transport::Http { url, token } => {
                Ok(Transport::Http(HttpSession::new(url, token.clone())))
            }
        }
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        match self {
            Transport::Stdio { session, .. } => session.request(method, params).await,
            Transport::Http(session) => session.request(method, params).await,
        }
    }

    pub async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        match self {
            Transport::Stdio { session, .. } => session.notify(method, params).await,
            Transport::Http(session) => session.notify(method, params).await,
        }
    }

    pub async fn initialize(&mut self) -> Result<Value, String> {
        match self {
            Transport::Stdio { session, .. } => session.initialize().await,
            Transport::Http(session) => session.initialize().await,
        }
    }

    pub async fn list_tools(&mut self) -> Result<Vec<ToolMeta>, String> {
        match self {
            Transport::Stdio { session, .. } => session.list_tools().await,
            Transport::Http(session) => session.list_tools().await,
        }
    }

    /// 调一次工具，返回给模型读的文本。
    pub async fn call_tool(&mut self, tool: &str, args: Value) -> Result<String, String> {
        match self {
            Transport::Stdio { session, .. } => session.call_tool(tool, args).await,
            Transport::Http(session) => session.call_tool(tool, args).await,
        }
    }
}
