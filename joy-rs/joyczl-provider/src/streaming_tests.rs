//! End-to-end streaming tests: spin up a local fake HTTP server feeding SSE and let a
//! real HTTP client connect — validating the parse, transport, and callback chain together.
//! Closer to reality than feeding bytes to `data_lines`, and needs no API key at all.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::{Provider, StopReason};

/// Start a server that answers once, returning its address.
/// The caller supplies the body — the same server feeds anthropic or openai wire formats.
fn spawn_sse_server(body: String) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the port");
    let addr = listener.local_addr().expect("the address").to_string();
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept the connection");
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf); // reading the request head is enough; no need to parse
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
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\", world\"}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    ]
    .concat()
}

fn openai_sse_body() -> String {
    [
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"he\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"llo\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    ]
    .concat()
}

fn collect_deltas() -> (Arc<Mutex<Vec<String>>>, crate::TextSink) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let sink: crate::TextSink = Arc::new(move |delta: &str| {
        captured.lock().expect("lock").push(delta.to_string());
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
                cache_prefix: None,
                model: "claude-sonnet-5".to_string(),
                system: None,
                messages: vec![crate::Message::user_text("hi")],
                tools: vec![],
                max_tokens: 64,
            },
            on_text,
        )
        .await
        .expect("the streaming call succeeds");

    assert_eq!(
        *deltas.lock().unwrap(),
        vec!["hello".to_string(), ", world".to_string()]
    );
    assert_eq!(
        response.text(),
        "hello, world",
        "the final reply is still the full text"
    );
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
                cache_prefix: None,
                model: "gpt-5.5".to_string(),
                system: None,
                messages: vec![crate::Message::user_text("hi")],
                tools: vec![],
                max_tokens: 64,
            },
            on_text,
        )
        .await
        .expect("the streaming call succeeds");

    assert_eq!(
        *deltas.lock().unwrap(),
        vec!["he".to_string(), "llo".to_string()]
    );
    assert_eq!(response.text(), "hello");
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(response.usage.input_tokens, 7);
    assert_eq!(response.usage.output_tokens, 2);
}

#[test]
fn thought_signature_round_trips_through_the_openai_wire() {
    // Gemini thinking models' signature: returned this turn, must go back verbatim or 400.
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
        crate::openai::from_openai(&serde_json::to_string(&body).unwrap()).expect("parse");
    let extra = response
        .content
        .iter()
        .find_map(|b| match b {
            crate::ContentBlock::ToolUse { extra, .. } => extra.clone(),
            _ => None,
        })
        .expect("the tool call should carry extra_content");
    assert_eq!(extra.get("thought_signature"), Some(&json!("sig-abc")));

    // In the next message, the signature must appear on the outgoing extra_content.
    let request = crate::CreateRequest {
        cache_prefix: None,
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
        .expect("the signature should come back");
    assert_eq!(signature, "sig-abc");
}
