//! The subagent's two hardening measures, run end to end:
//! structured results (validation + one retry + fallback) and the "the report is a retelling" wrapping.

use std::sync::Arc;

use joyczl_protocol::{RequestId, ServerNotification, TurnStartParams};
use joyczl_provider::mock::Mock;
use joyczl_provider::Resolved;
use serde_json::json;

use crate::{run_turn, EventSink, Frame};

/// Assemble a Server with the subagent enabled, fitted with the scripted model.
async fn server_with(
    responses: Vec<joyczl_provider::CreateResponse>,
) -> (crate::Server, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temporary directory");
    let settings = joyczl_config::Settings {
        home: dir.path().to_path_buf(),
        provider: "openai".to_string(),
        api_key: Some("test-key".to_string()),
        model: Some("test-model".to_string()),
        small_model: Some("test-model".to_string()),
        delegate_enabled: true,
        ..Default::default()
    };
    let server = crate::open(&settings).await.expect("装配");
    server.install_provider(Resolved {
        context_window: joyczl_provider::DEFAULT_CONTEXT_WINDOW,
        vision: false,
        provider_id: "mock".to_string(),
        client: Arc::new(Mock::new(responses)),
        model: "test-model".to_string(),
        small_model: "test-model".to_string(),
    });
    (server, dir)
}

/// Run one turn, returning (tool output, final reply).
async fn collect(server: &crate::Server, message: &str) -> (String, String) {
    let (sink, mut rx) = EventSink::channel();
    run_turn(
        server,
        TurnStartParams {
            attachments: None,
            session_id: Some("test".to_string()),
            message: message.to_string(),
            stream: Some(true),
            cwd: None,
            tier: None,
        },
        RequestId::Number(1),
        &sink,
    )
    .await
    .expect("这一轮该跑完");
    drop(sink);

    let mut tool_output = String::new();
    let mut reply = String::new();
    while let Ok(frame) = rx.try_recv() {
        if let Frame::Notification(notification) = frame {
            match notification {
                ServerNotification::ToolCompleted(done) => tool_output = done.output.clone(),
                ServerNotification::TurnCompleted(done) => reply = done.reply.clone(),
                _ => {}
            }
        }
    }
    (tool_output, reply)
}

#[tokio::test]
async fn a_structured_result_is_validated_and_retried_once() {
    let (server, _dir) = server_with(vec![
        // Retrieval gate
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "delegate"}"#),
        // Parent turn: delegate the task and ask for a structured conclusion
        Mock::tool_use(
            "call-1",
            "delegate_task",
            json!({
                "task": "算一下 2+2",
                "result_schema": {
                    "type": "object",
                    "required": ["answer"],
                    "properties": {"answer": {"type": "integer"}}
                }
            }),
        ),
        // Subagent first attempt: prose (non-compliant)
        Mock::text("我觉得是四吧。"),
        // Subagent second attempt (retry with the error attached): compliant JSON
        Mock::text(r#"{"answer": 4}"#),
        // Parent turn wind-down
        Mock::text("子代理说答案是 4。"),
    ])
    .await;

    let (tool_output, reply) = collect(&server, "派个子代理去算 2+2").await;

    assert!(reply.contains("4"), "{reply}");
    assert!(
        tool_output.contains("reported"),
        "报告要标明是转述：{tool_output}"
    );
    assert!(
        tool_output.contains(r#"{"answer":4}"#),
        "合规之后带回的该是紧凑 JSON：{tool_output}"
    );
    assert!(
        !tool_output.contains("我觉得是四吧"),
        "不合规的那一版不该留在结论里：{tool_output}"
    );
}

#[tokio::test]
async fn a_structured_result_falls_back_to_prose_after_two_failures() {
    let (server, _dir) = server_with(vec![
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "delegate"}"#),
        Mock::tool_use(
            "call-1",
            "delegate_task",
            json!({
                "task": "算一下 2+2",
                "result_schema": {
                    "type": "object",
                    "required": ["answer"],
                    "properties": {"answer": {"type": "integer"}}
                }
            }),
        ),
        Mock::text("四。"),     // First attempt: not JSON
        Mock::text("还是四。"), // Second attempt: still non-compliant
        Mock::text("它说是四。"),
    ])
    .await;

    let (tool_output, _) = collect(&server, "派个子代理去算 2+2").await;

    assert!(
        tool_output.contains("failed validation twice"),
        "两次都不合规要如实说出来：{tool_output}"
    );
    assert!(
        tool_output.contains("四。"),
        "回落时保留它的原始文字（不让活白跑）：{tool_output}"
    );
}
