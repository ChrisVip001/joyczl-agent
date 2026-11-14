use super::*;
use crate::{Message, ToolSchema};

fn request() -> CreateRequest {
    CreateRequest {
        model: "test-model".into(),
        cache_prefix: Some("stable".into()),
        system: Some("tail".into()),
        messages: vec![Message::user_text("work")],
        tools: vec![ToolSchema {
            name: "lookup".into(),
            description: "lookup".into(),
            input_schema: json!({"type":"object","properties":{}}),
        }],
        max_tokens: 600,
    }
}
fn tool_response() -> Value {
    json!({"status":"completed","output":[
        {"type":"reasoning","id":"reason-1","summary":[],"encrypted_content":"opaque-reasoning"},
        {"type":"function_call","id":"item-1","call_id":"call-1","name":"lookup","arguments":"{}","status":"completed"}
    ],"usage":{"input_tokens":100,"output_tokens":20,"input_tokens_details":{"cached_tokens":80}}})
}

#[test]
fn stateless_tool_round_trip_preserves_reasoning_and_native_call_identity() {
    let response = from_responses(&tool_response()).unwrap();
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(response.tool_uses()[0].0, "call-1");
    assert_eq!(response.usage.cache_read_input_tokens, 80);
    let mut request = request();
    request.messages.push(Message {
        role: Role::Assistant,
        content: response.content,
    });
    request.messages.push(Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "call-1".into(),
            content: "found".into(),
        }],
    });
    // Checkpoints serialize these blocks; reconstruction must remain lossless.
    request.messages =
        serde_json::from_str(&serde_json::to_string(&request.messages).unwrap()).unwrap();
    let body = to_responses(&request).unwrap();
    assert_eq!(body["store"], false);
    assert_eq!(body["instructions"], "stable\n\ntail");
    assert_eq!(body["include"][0], "reasoning.encrypted_content");
    assert_eq!(body["input"][1], tool_response()["output"][0]);
    assert_eq!(body["input"][2], tool_response()["output"][1]);
    assert_eq!(body["input"][3]["call_id"], "call-1");
    assert_eq!(body["input"][3]["type"], "function_call_output");
    assert_eq!(body["tools"][0]["strict"], false);
    assert!(crate::reject_responses_continuation(&request).is_err());
}

#[test]
fn incomplete_or_malformed_function_calls_never_become_executable_tools() {
    let mut response = tool_response();
    response["output"][1]["status"] = json!("in_progress");
    assert!(from_responses(&response).is_err());
    response["output"][1]["status"] = json!("completed");
    response["output"][1]["arguments"] = json!("{broken");
    assert!(from_responses(&response).is_err());
    response = tool_response();
    response["output"][0]
        .as_object_mut()
        .unwrap()
        .remove("encrypted_content");
    assert!(from_responses(&response).is_err());
}

#[test]
fn text_images_refusals_and_output_limits_keep_their_meaning() {
    let mut req = request();
    req.messages[0].content.push(ContentBlock::Image {
        media_type: "image/png".into(),
        data: "YWJj".into(),
    });
    assert_eq!(
        to_responses(&req).unwrap()["input"][0]["content"][1]["image_url"],
        "data:image/png;base64,YWJj"
    );
    let response = from_responses(&json!({"status":"incomplete","output":[{"type":"message","content":[{"type":"output_text","text":"partial"}]}]})).unwrap();
    assert_eq!(response.stop_reason, StopReason::MaxTokens);
    assert_eq!(response.text(), "partial");
    assert_eq!(from_responses(&json!({"status":"completed","output":[{"type":"message","content":[{"type":"refusal","refusal":"cannot help"}]}]})).unwrap().text(), "cannot help");
}

async fn endpoint(
    body: String,
    content_type: &'static str,
) -> (String, tokio::task::JoinHandle<Value>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let split = loop {
            let mut chunk = [0; 4096];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(split) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                break split + 4;
            }
        };
        let header = String::from_utf8_lossy(&bytes[..split]);
        assert!(header.starts_with("POST /responses "));
        let length: usize = header
            .lines()
            .find_map(|line| {
                line.to_lowercase()
                    .strip_prefix("content-length:")
                    .map(|n| n.trim().parse().unwrap())
            })
            .unwrap();
        while bytes.len() < split + length {
            let mut chunk = [0; 4096];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
        }
        let request: Value = serde_json::from_slice(&bytes[split..split + length]).unwrap();
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        request
    });
    (base, task)
}

#[tokio::test]
async fn streaming_waits_for_a_terminal_response_and_preserves_tools() {
    let terminal = tool_response();
    let sse = format!(
        "data: {}\n\ndata: {}\n\n",
        json!({"type":"response.output_text.delta","delta":"hello"}),
        json!({"type":"response.completed","response":terminal})
    );
    let (base, server) = endpoint(sse, "text/event-stream").await;
    let client = Client::new("", Some(&base), Duration::from_secs(3), 0);
    let text = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let captured = text.clone();
    let result = client
        .stream(
            request(),
            std::sync::Arc::new(move |delta| captured.lock().unwrap().push_str(delta)),
        )
        .await
        .unwrap();
    assert_eq!(result.tool_uses().len(), 1);
    assert_eq!(*text.lock().unwrap(), "hello");
    assert_eq!(server.await.unwrap()["stream"], true);
    let (base, server) = endpoint(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n".into(),
        "text/event-stream",
    )
    .await;
    let client = Client::new("", Some(&base), Duration::from_secs(3), 0);
    assert!(client
        .stream(request(), std::sync::Arc::new(|_| {}))
        .await
        .is_err());
    server.await.unwrap();
}

#[test]
fn configured_reasoning_only_applies_to_its_main_model() {
    let client = Client::new("", None, Duration::from_secs(1), 0)
        .with_reasoning("test-model", Some("high".into()));
    let mut request = request();
    assert_eq!(
        client.body(&request).unwrap()["reasoning"]["effort"],
        "high"
    );
    request.model = "different-small-model".into();
    assert!(client.body(&request).unwrap().get("reasoning").is_none());
    let client = Client::new("", None, Duration::from_secs(1), 0);
    assert!(client.body(&request).unwrap().get("reasoning").is_none());
}
