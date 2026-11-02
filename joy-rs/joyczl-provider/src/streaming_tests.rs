//! 流式的端到端测试：起一个本地假 HTTP 服务喂 SSE，
//! 让真的 HTTP 客户端去连 —— 解析、传输、回调这条链一起验证。
//! 比喂字节流给 `data_lines` 更接近真实情况，而且不需要任何 API key。

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::{Provider, StopReason};

/// 起一个只应答一次的服务，返回它的地址。
/// 应答体由调用方给 —— 同一个服务可以喂 anthropic 或 openai 两种格式。
fn spawn_sse_server(body: String) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定端口");
    let addr = listener.local_addr().expect("地址").to_string();
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("接受连接");
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf); // 读到请求头就够，不必解析
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = socket.write_all(response.as_bytes());
        let _ = socket.flush();
    });
    format!("http://{addr}")
}

fn anthropic_sse_body() -> String {
    [
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你好\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"，世界\"}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    ]
    .concat()
}

fn openai_sse_body() -> String {
    [
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"你\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"好\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    ]
    .concat()
}

fn collect_deltas() -> (Arc<Mutex<Vec<String>>>, crate::TextSink) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let sink: crate::TextSink = Arc::new(move |delta: &str| {
        captured.lock().expect("锁").push(delta.to_string());
    });
    (seen, sink)
}

#[tokio::test]
async fn anthropic_stream_yields_deltas_over_a_real_socket() {
    let base = spawn_sse_server(anthropic_sse_body());
    let client = crate::anthropic::Client::new(
        "test-key",
        Some(&base),
        std::time::Duration::from_secs(5),
        2,
    );
    let (deltas, on_text) = collect_deltas();

    let response = client
        .stream(
            crate::CreateRequest {
                model: "claude-sonnet-5".to_string(),
                system: None,
                messages: vec![crate::Message::user_text("hi")],
                tools: vec![],
                max_tokens: 64,
            },
            on_text,
        )
        .await
        .expect("流式调用成功");

    assert_eq!(
        *deltas.lock().unwrap(),
        vec!["你好".to_string(), "，世界".to_string()]
    );
    assert_eq!(response.text(), "你好，世界", "最终应答仍是完整文本");
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(response.usage.input_tokens, 10);
    assert_eq!(response.usage.output_tokens, 5);
}

#[tokio::test]
async fn openai_stream_yields_deltas_over_a_real_socket() {
    let base = spawn_sse_server(openai_sse_body());
    let client = crate::openai::Client::new(
        "test-key",
        Some(&base),
        std::time::Duration::from_secs(5),
        2,
    );
    let (deltas, on_text) = collect_deltas();

    let response = client
        .stream(
            crate::CreateRequest {
                model: "gpt-5.5".to_string(),
                system: None,
                messages: vec![crate::Message::user_text("hi")],
                tools: vec![],
                max_tokens: 64,
            },
            on_text,
        )
        .await
        .expect("流式调用成功");

    assert_eq!(
        *deltas.lock().unwrap(),
        vec!["你".to_string(), "好".to_string()]
    );
    assert_eq!(response.text(), "你好");
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(response.usage.input_tokens, 7);
    assert_eq!(response.usage.output_tokens, 2);
}

#[test]
fn thought_signature_round_trips_through_the_openai_wire() {
    // Gemini 思考模型的签名：这轮带回来，下轮必须原样送回去，否则 400。
    let body = json!({
        "choices": [{"message": {
            "content": null,
            "tool_calls": [{"id": "call_1", "type": "function",
                            "function": {"name": "save_note", "arguments": "{\"subject\":\"a\"}"},
                            "extra_content": {"thought_signature": "sig-abc"}}]
        }}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
    });
    let response =
        crate::openai::from_openai(&serde_json::to_string(&body).unwrap()).expect("解析");
    let extra = response
        .content
        .iter()
        .find_map(|b| match b {
            crate::ContentBlock::ToolUse { extra, .. } => extra.clone(),
            _ => None,
        })
        .expect("tool 调用应当带上 extra_content");
    assert_eq!(extra.get("thought_signature"), Some(&json!("sig-abc")));

    // 下一条消息里，签名要出现在发出去的 extra_content 上。
    let request = crate::CreateRequest {
        model: "gemini-3.5-flash".to_string(),
        system: None,
        messages: vec![
            crate::Message {
                role: crate::Role::Assistant,
                content: response.content,
            },
            crate::Message {
                role: crate::Role::User,
                content: vec![crate::ContentBlock::ToolResult {
                    tool_use_id: "call_1".to_string(),
                    content: "ok".to_string(),
                }],
            },
        ],
        tools: vec![],
        max_tokens: 64,
    };
    let wire = crate::openai::to_openai(&request);
    let signature = wire["messages"][0]["tool_calls"][0]["extra_content"]["thought_signature"]
        .as_str()
        .expect("签名应当被带回去");
    assert_eq!(signature, "sig-abc");
}
