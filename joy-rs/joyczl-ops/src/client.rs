//! app-server 的客户端：起一个子进程，走 stdio 上的换行分隔 JSON-RPC。
//!
//! 这一层刻意做得很薄 —— 协议本身已经说清了一切，客户端只剩下三件事：
//!
//!   1. 给每条请求配一个 id，把「等待应答的人」记在表里；
//!   2. 读 stdout 的每一行，是应答就交给等在表里的那个人，
//!      是通知就播给它该去的地方（驾驶舱的 SSE）；
//!   3. 进程走了就把表里所有人叫醒，别让他们等到 HTTP 超时。
//!
//! **失败一律原样传回去**：`ErrorObject` 是 app-server 说的话，这里不加工。
//! 「模型还没配好：缺 API key，往 .env 里加…」这种句子，换成一句
//! 「dashboard 请求失败」就把唯一有用的信息扔了。

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use joyczl_protocol::{
    codes, ErrorObject, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    RequestId, ServerNotification, JSONRPC_VERSION,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, oneshot, Mutex as AsyncMutex};

/// 还没被读走的通知最多攒多少条。一小时的对话也够用；真满了说明订阅者
/// （某个卡住的浏览器标签页）已经不看了，那时丢掉旧的通知是对的。
const EVENT_BUFFER: usize = 256;

/// 读 stdout 的循环和发请求的一方共享的东西。
///
/// `Mutex`（std 的、不是 tokio 的）：这两处都只是插一下 / 取一下，没有
/// await 夹在中间，用异步锁反而会把「同一时刻只有一个 writer」这件事变复杂。
struct Shared {
    pending: Mutex<HashMap<RequestId, oneshot::Sender<Result<Value, ErrorObject>>>>,
    events: broadcast::Sender<ServerNotification>,
}

/// 一个跑着的 app-server，外加跟它说话的两根管子。
pub struct AppServer {
    /// 子进程。测试里是 `None` —— 那时两条管子接的是内存里的假服务端。
    ///
    /// 字段留着不只是为了 drop 时杀进程：`kill_on_drop` 是给
    /// `joy dashboard` 按 Ctrl-C 之后不留孤儿进程用的。
    _process: Option<Child>,
    stdin: AsyncMutex<Box<dyn AsyncWrite + Unpin + Send>>,
    shared: Arc<Shared>,
    next_id: AtomicI32,
}

impl AppServer {
    /// 起一个 app-server 子进程。
    ///
    /// 二进制是 `joy` 自己（`current_exe()`）：`joy dashboard` 和
    /// `joy app-server` 本来就是同一个可执行文件的子命令，fork 自己比去
    /// PATH 里找一个同名二进制可靠 —— 后者可能是另一个版本，甚至不存在。
    pub fn spawn(home: &Path) -> Result<Self> {
        let exe = std::env::current_exe().context("拿不到自己的可执行文件路径")?;
        let mut command = Command::new(exe);
        command
            .arg("app-server")
            // JOY_HOME 显式传下去：驾驶舱页面上写着「记忆在哪」，
            // 子进程必须读同一份，否则页面显示的是另一个 home 的记忆 ——
            // 「我的记忆怎么没了」这种问题最难查。
            .env("JOY_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // 子进程的日志直接落到我们的 stderr。stdout 是协议通道，
            // 一行日志混进去就是一帧读不懂的 JSON。
            .stderr(Stdio::inherit())
            .kill_on_drop(true);

        let mut child = command.spawn().context("起 app-server 子进程失败")?;
        let stdin = child.stdin.take().expect("刚 piped 的 stdin");
        let stdout = child.stdout.take().expect("刚 piped 的 stdout");
        Ok(Self::attach(Some(child), stdout, stdin))
    }

    /// 接上两条管子。真实路径和测试路径的区别只有「有没有子进程」。
    fn attach(
        process: Option<Child>,
        reader: impl AsyncRead + Unpin + Send + 'static,
        writer: impl AsyncWrite + Unpin + Send + 'static,
    ) -> Self {
        let shared = Arc::new(Shared {
            pending: Mutex::new(HashMap::new()),
            events: broadcast::channel(EVENT_BUFFER).0,
        });
        tokio::spawn(read_loop(Arc::clone(&shared), reader));
        Self {
            _process: process,
            stdin: AsyncMutex::new(Box::new(writer)),
            shared,
            next_id: AtomicI32::new(1),
        }
    }

    /// 发一条请求，等它的应答。
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, ErrorObject> {
        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending().insert(id.clone(), tx);

        let frame = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.clone(),
            method: method.to_string(),
            params: Some(params),
        };
        let mut line =
            serde_json::to_string(&frame).map_err(|e| internal(format!("请求装不进 JSON：{e}")))?;
        line.push('\n');

        if let Err(error) = self.write(line.as_bytes()).await {
            // 没写出去就撤掉那一行，否则它会一直挂在表里等一个永远不来的应答。
            self.pending().remove(&id);
            return Err(transport(&format!("往 app-server 写请求失败：{error}")));
        }

        match rx.await {
            Ok(result) => result,
            // 发送端没了，只有一种可能：读循环在叫醒我之前就退出了（连接断了）。
            Err(_) => Err(transport("app-server 走了，没等到应答")),
        }
    }

    /// 同 `call`，但把应答解成具体类型。
    pub async fn request<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T, ErrorObject> {
        let value = self.call(method, params).await?;
        serde_json::from_value(value).map_err(|e| internal(format!("{method} 的应答看不懂：{e}")))
    }

    /// 订阅服务端推送。每个浏览器连接订一次。
    ///
    /// 必须在**发请求之前**调用：`turn/start` 的应答要等整轮跑完，
    /// 而 `textDelta` 是随发生随发的 —— 先发请求再订阅，最早的几个增量
    /// 就会落在「还没人听」的空当里，页面上表现为「开头少了几个字」。
    pub fn subscribe(&self) -> broadcast::Receiver<ServerNotification> {
        self.shared.events.subscribe()
    }

    async fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(bytes).await?;
        stdin.flush().await
    }

    fn pending(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<RequestId, oneshot::Sender<Result<Value, ErrorObject>>>>
    {
        self.shared.pending.lock().expect("pending 表没被毒化")
    }
}

/// 读 stdout：一行一帧，直到子进程关掉 stdout（它走了）。
async fn read_loop<R: AsyncRead + Unpin>(shared: Arc<Shared>, reader: R) {
    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => accept(&shared, line.trim()),
            Ok(None) => break,
            Err(error) => {
                eprintln!("(joy) 读 app-server 的 stdout 出错：{error}");
                break;
            }
        }
    }

    // 叫醒所有还在等的人。不这么做的话，他们各自要等到自己的超时，
    // 而调用方看到的是「驾驶舱卡住了」，不是「app-server 挂了」。
    let waiting: Vec<_> = shared
        .pending
        .lock()
        .expect("pending 表没被毒化")
        .drain()
        .map(|(_, tx)| tx)
        .collect();
    for tx in waiting {
        let _ = tx.send(Err(transport("app-server 退出了，这条请求没有应答")));
    }
}

/// 收下一行。应答交给等的人，通知播出去，读不懂的记一句接着读。
fn accept(shared: &Shared, line: &str) {
    if line.is_empty() {
        return;
    }
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => {
            // 一行读不懂不该让整条连接断掉：否则一个编码 bug 在外面的表现
            // 是「什么都没发生」，比报错难查得多。
            eprintln!("(joy) app-server 送来一行读不懂的东西：{error}");
            return;
        }
    };

    if value.get("method").is_some() {
        // stdout 上只有 `turn/notification` 这一种方法，参数就是那一帧通知。
        let params = serde_json::from_value::<JsonRpcNotification>(value)
            .ok()
            .and_then(|envelope| envelope.params);
        if let Some(params) = params {
            match serde_json::from_value::<ServerNotification>(params) {
                // 没人在听的时候 send 会返回 Err（驾驶舱没开页面），这不是错。
                Ok(notification) => {
                    let _ = shared.events.send(notification);
                }
                Err(error) => eprintln!("(joy) 这条通知读不懂：{error}"),
            }
        }
        return;
    }

    match serde_json::from_value::<JsonRpcMessage>(value) {
        Ok(JsonRpcMessage::Response(JsonRpcResponse { id, result, .. })) => {
            if let Some(tx) = shared
                .pending
                .lock()
                .expect("pending 表没被毒化")
                .remove(&id)
            {
                let _ = tx.send(Ok(result));
            }
        }
        Ok(JsonRpcMessage::Error(error)) => match error.id {
            Some(id) => {
                if let Some(tx) = shared
                    .pending
                    .lock()
                    .expect("pending 表没被毒化")
                    .remove(&id)
                {
                    let _ = tx.send(Err(error.error));
                }
            }
            // id 为 null：请求本身就是坏 JSON，没有人在等。但它确实出了事，
            // 所以照样播出去，而不是悄悄吞掉。
            None => {
                let _ = shared.events.send(crate::as_notification(error.error));
            }
        },
        Err(error) => eprintln!("(joy) 这一帧既不是通知也不是应答：{error}"),
    }
}

fn internal(message: String) -> ErrorObject {
    ErrorObject {
        code: codes::INTERNAL_ERROR,
        message,
        data: None,
    }
}

fn transport(what: &str) -> ErrorObject {
    internal(format!("跟 app-server 的连接断了：{what}"))
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod client_tests;
