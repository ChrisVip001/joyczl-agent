//! stdio 传输：一行一条 JSON-RPC 消息。
//!
//! 选换行分隔 JSON 而不是 Content-Length 分帧，是因为它既能被
//! `echo '…' | joy app-server` 手工调试，也能被任何语言的三行代码接上
//! （写一行、读一行）。LSP/MCP 那套分帧带来的复杂度，在这个规模上
//! 换不到对等的好处。
//!
//! **stdout 只有一个 writer 在写。** 请求循环把通知和应答都塞进通道，
//! writer 按序落盘并逐帧 flush —— 流式的 `textDelta` 因此能一条一条
//! 到达浏览器，而不会跟应答交错或写坏。

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

    // 唯一的 stdout writer。
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
                // 客户端断了：writer 退出，通道里的剩余帧随之丢弃。
                break;
            }
        }
    });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // 解析不了请求时 id 无从得知（JSON-RPC 规定此时 id 为 null），
        // 所以 parse error 走单独一支。
        let sink = EventSink::new(tx.clone());
        match serde_json::from_str::<JsonRpcRequest>(line) {
            Ok(request) => {
                if request.method == joyczl_protocol::methods::TURN_START {
                    // 只有 turn/start 需要并发：它一跑几十秒，read 循环必须
                    // 继续读 —— 否则 turn/interrupt 这类快请求永远进不来，
                    // 打断也就无从谈起。
                    let server = server.clone();
                    tokio::spawn(async move {
                        handle(&server, request, sink).await;
                    });
                } else {
                    // 其余方法照旧按序处理：脚本一行一行喂进来时，「上一句
                    // 的写入下一句查得到」是合理预期，乱序会让冒烟测试里
                    // 的 remember → search → forget 互相踩脚。
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

    // 请求循环结束（stdin 关闭）：放手让 writer 把通道里剩的帧写完再退。
    drop(tx);
    let _ = writer.await;
    Ok(())
}
