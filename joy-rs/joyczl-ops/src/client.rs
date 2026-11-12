//! Client of the app-server: spawn a subprocess, speak newline-delimited JSON-RPC over stdio.
//!
//! This layer is deliberately thin — the protocol says it all; the client has three jobs:
//!
//!   1. Give each request an id and record "who is waiting for the answer" in a table;
//!   2. Read every line of stdout: hand answers to whoever waits in the table, and
//!      broadcast notifications where they belong (the dashboard's SSE);
//!   3. When the process dies, wake everyone in the table; don't let them wait for timeouts.
//!
//! **Failures are passed back verbatim**: the `ErrorObject` is what app-server said; no
//! rewording here. A sentence like "model not configured: missing API key, add it to .env…"
//! carries the only useful information; "dashboard request failed" would throw it away.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
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

/// How many not-yet-consumed notifications may pile up. Enough for an hour-long chat; if
/// it truly fills, a subscriber (some stuck browser tab) has stopped looking — drop old ones.
const EVENT_BUFFER: usize = 256;

/// What the stdout read loop and the request senders share.
///
/// `Mutex` (std's, not tokio's): both sides only insert / take, with no await in between;
/// an async lock would only complicate "one writer at a time".
struct Shared {
    pending: Mutex<HashMap<RequestId, oneshot::Sender<Result<Value, ErrorObject>>>>,
    events: broadcast::Sender<ServerNotification>,
}

/// A running app-server, plus the two pipes for talking to it.
pub struct AppServer {
    /// The home the child was told to use — ops may read the same files (connectors.json)
    /// without asking the child, because it is the same machine and the same truth.
    home: PathBuf,
    /// Child process. `None` in tests — the pipes then connect to an in-memory fake server.
    ///
    /// The field stays not only to kill the process on drop: `kill_on_drop` is so that
    /// Ctrl-C on `joy dashboard` leaves no orphan processes behind.
    _process: Option<Child>,
    stdin: AsyncMutex<Box<dyn AsyncWrite + Unpin + Send>>,
    shared: Arc<Shared>,
    next_id: AtomicI32,
}

impl AppServer {
    /// Spawn an app-server subprocess.
    ///
    /// The binary is `joy` itself (`current_exe()`): `joy dashboard` and `joy app-server`
    /// are subcommands of the same executable; forking yourself is more reliable than
    /// searching PATH for a same-named binary — that one may be another version, or absent.
    ///
    /// `interactive` = a human is looking at a single-user dashboard. That is the capability
    /// switch: writing files is enabled there, but every write still needs an approval card
    /// in the chat window, and only inside the working directory the human picked. Hub users
    /// and headless `joy app-server` keep the conservative defaults (no write, never ask).
    /// A human-set environment variable always wins.
    pub fn spawn(home: &Path, interactive: bool) -> Result<Self> {
        let home = home.to_path_buf();
        let exe =
            std::env::current_exe().context("could not get the path of our own executable")?;
        let mut command = Command::new(exe);
        command
            .arg("app-server")
            // JOY_HOME is passed down explicitly: the dashboard page shows "where memory
            // is", and the child must read the same copy, or the page shows another home's
            // memory — "where did my memory go" is the hardest class of bug to trace.
            .env("JOY_HOME", &home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // The child's logs go straight to our stderr. stdout is the protocol channel;
            // one log line mixed in would be a frame of unreadable JSON.
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if interactive {
            if std::env::var_os("JOY_WRITE").is_none() {
                command.env("JOY_WRITE", "1");
            }
            if std::env::var_os("JOY_APPROVAL").is_none() {
                command.env("JOY_APPROVAL", "on-request");
            }
        }

        let mut child = command
            .spawn()
            .context("could not start the app-server child process")?;
        let stdin = child.stdin.take().expect("the stdin just piped");
        let stdout = child.stdout.take().expect("the stdout just piped");
        Ok(Self::attach(home, Some(child), stdout, stdin))
    }

    /// Attach the two pipes. Real and test paths differ only in "is there a child process".
    fn attach(
        home: PathBuf,
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
            home,
            _process: process,
            stdin: AsyncMutex::new(Box::new(writer)),
            shared,
            next_id: AtomicI32::new(1),
        }
    }

    /// The home this child serves (connectors, memory — one machine, one truth).
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Send a request and wait for its answer.
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
        let mut line = serde_json::to_string(&frame)
            .map_err(|e| internal(format!("the request does not fit into JSON: {e}")))?;
        line.push('\n');

        if let Err(error) = self.write(line.as_bytes()).await {
            // If the write failed, remove the entry, or it waits forever for an absent answer.
            self.pending().remove(&id);
            return Err(transport(&format!(
                "could not write the request to the app-server: {error}"
            )));
        }

        match rx.await {
            Ok(result) => result,
            // Sender gone: only one possibility — the read loop exited before waking me (lost link).
            Err(_) => Err(transport("the app-server went away before answering")),
        }
    }

    /// Same as `call`, but deserializes the answer into a concrete type.
    pub async fn request<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T, ErrorObject> {
        let value = self.call(method, params).await?;
        serde_json::from_value(value)
            .map_err(|e| internal(format!("the reply to {method} could not be read: {e}")))
    }

    /// Subscribe to server pushes. Each browser connection subscribes once.
    ///
    /// Must be called **before sending the request**: the `turn/start` answer waits for
    /// the whole turn, while `textDelta` is sent as it happens — subscribing after the
    /// request drops the first deltas into "nobody listening", showing as missing words.
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
        self.shared
            .pending
            .lock()
            .expect("the pending table is not poisoned")
    }
}

/// Read stdout: one line per frame, until the child closes stdout (it's gone).
async fn read_loop<R: AsyncRead + Unpin>(shared: Arc<Shared>, reader: R) {
    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => accept(&shared, line.trim()),
            Ok(None) => break,
            Err(error) => {
                eprintln!("(joy) error reading the app-server's stdout: {error}");
                break;
            }
        }
    }

    // Wake everyone still waiting. Otherwise each waits out its own timeout, and the
    // caller sees "the dashboard is stuck", not "app-server died".
    let waiting: Vec<_> = shared
        .pending
        .lock()
        .expect("the pending table is not poisoned")
        .drain()
        .map(|(_, tx)| tx)
        .collect();
    for tx in waiting {
        let _ = tx.send(Err(transport(
            "the app-server exited; this request has no reply",
        )));
    }
}

/// Take a line. Answers go to the waiter, notifications broadcast, garbage logs and move on.
fn accept(shared: &Shared, line: &str) {
    if line.is_empty() {
        return;
    }
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => {
            // One unreadable line must not kill the connection: otherwise a decoding bug
            // shows outside as "nothing happened", far harder to trace than an error.
            eprintln!("(joy) the app-server sent a line that could not be read: {error}");
            return;
        }
    };

    if value.get("method").is_some() {
        // stdout carries only `turn/notification`; the params are the notification frame.
        let params = serde_json::from_value::<JsonRpcNotification>(value)
            .ok()
            .and_then(|envelope| envelope.params);
        if let Some(params) = params {
            match serde_json::from_value::<ServerNotification>(params) {
                // With no listener, send returns Err (dashboard page closed) — not an error.
                Ok(notification) => {
                    let _ = shared.events.send(notification);
                }
                Err(error) => eprintln!("(joy) this notification could not be read: {error}"),
            }
        }
        return;
    }

    match serde_json::from_value::<JsonRpcMessage>(value) {
        Ok(JsonRpcMessage::Response(JsonRpcResponse { id, result, .. })) => {
            if let Some(tx) = shared
                .pending
                .lock()
                .expect("the pending table is not poisoned")
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
                    .expect("the pending table is not poisoned")
                    .remove(&id)
                {
                    let _ = tx.send(Err(error.error));
                }
            }
            // id null: the request itself was bad JSON; nobody is waiting. But something
            // did go wrong, so broadcast it instead of swallowing it silently.
            None => {
                let _ = shared.events.send(crate::as_notification(error.error));
            }
        },
        Err(error) => eprintln!("(joy) this frame is neither a notification nor a reply: {error}"),
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
    internal(format!("the connection to the app-server broke: {what}"))
}

#[cfg(test)]
#[path = "client_tests.rs"]
pub(crate) mod client_tests;
