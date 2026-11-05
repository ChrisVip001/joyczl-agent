//! The stdio transport: one JSON-RPC message per line.
//!
//! Newline-delimited JSON was chosen over Content-Length framing because it can be hand-debugged with
//! `echo '…' | joy app-server`, and connected from any language in three lines of code
//! (write a line, read a line). The complexity of LSP/MCP-style framing buys nothing comparable
//! at this scale.
//!
//! **stdout has exactly one writer.** The request loop pushes notifications and responses into the
//! channel, and the writer persists them in order, flushing frame by frame — so the streamed
//! `textDelta`s arrive at the browser one by one, without interleaving with responses or corrupting lines.

use anyhow::Result;
use joyczl_protocol::{
    codes, ErrorObject, JsonRpcError, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
    JSONRPC_VERSION,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::{handle, EventSink, Frame, Server};

pub async fn run_stdio(server: Server) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();

    // The one and only stdout writer.
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(frame) = rx.recv().await {
            let text = match frame {
                Frame::Notification(notification) => {
                    let envelope = JsonRpcNotification {
                        jsonrpc: JSONRPC_VERSION.to_string(),
                        method: "turn/notification".to_string(),
                        params: Some(serde_json::to_value(&notification).expect("序列化")),
                    };
                    serde_json::to_string(&envelope).expect("序列化")
                }
                Frame::Response(response) => serde_json::to_string(&response).expect("序列化"),
            };
            if stdout.write_all(text.as_bytes()).await.is_err()
                || stdout.write_all(b"\n").await.is_err()
                || stdout.flush().await.is_err()
            {
                // The client is gone: the writer exits, and remaining frames in the channel are dropped with it.
                break;
            }
        }
    });

    // Startup reconciliation: open checkpoints (a crashed process's mid-turn leftovers) resume
    // as their own tasks, notifications flowing out the same stdout — whoever connects (hub,
    // dashboard) sees "resumed" like any other turn event.
    {
        let server = server.clone();
        let sink = EventSink::new(tx.clone());
        tokio::spawn(async move { crate::resume::reconcile(&server, &sink).await });
    }

    // Inbound mail: only when connectors.json has an `imap` block — otherwise this returns
    // immediately and the process looks exactly as before.
    {
        let sink = EventSink::new(tx.clone());
        crate::inbox::spawn(&server, &sink);
    }

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // When the request cannot be parsed, the id is unknowable (JSON-RPC says id is null then),
        // so parse errors take their own branch.
        let sink = EventSink::new(tx.clone());
        match serde_json::from_str::<JsonRpcRequest>(line) {
            Ok(request) => {
                if request.method == joyczl_protocol::methods::TURN_START {
                    // Only turn/start needs concurrency: it runs for tens of seconds, so the read loop must
                    // keep reading — otherwise fast requests like turn/interrupt never get in, and
                    // interrupting becomes impossible.
                    let server = server.clone();
                    tokio::spawn(async move {
                        handle(&server, request, sink).await;
                    });
                } else {
                    // The other methods are handled in order as before: when a script feeds lines one by one,
                    // "the previous line's write is visible to the next line's query" is a fair expectation;
                    // reordering would make remember → search → forget in smoke tests step on each other.
                    handle(&server, request, sink).await;
                }
            }
            Err(e) => sink.response(JsonRpcMessage::Error(JsonRpcError {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: None,
                error: ErrorObject {
                    code: codes::PARSE_ERROR,
                    message: format!("解析失败：{e}"),
                    data: None,
                },
            })),
        }
    }

    // The request loop is over (stdin closed): let go and let the writer finish the remaining frames before exiting.
    drop(tx);
    let _ = writer.await;
    Ok(())
}
