//! 子代理的两条加固，端到端跑一遍：
//! 结构化结果（校验 + 一次重试 + 回落）与「报告是转述」的包装。

use std::sync::Arc;

use joyczl_protocol::{RequestId, ServerNotification, TurnStartParams};
use joyczl_provider::mock::Mock;
use joyczl_provider::Resolved;
use serde_json::json;

use crate::{run_turn, EventSink, Frame};

/// 装配一个开了子代理的 Server，装上 scripted 模型。
async fn server_with(
    responses: Vec<joyczl_provider::CreateResponse>,
) -> (crate::Server, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("临时目录");
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
        provider_id: "mock".to_string(),
        client: Arc::new(Mock::new(responses)),
        model: "test-model".to_string(),
        small_model: "test-model".to_string(),
    });
    (server, dir)
}

/// 跑一轮，返回（工具输出, 最终回复）。
async fn collect(server: &crate::Server, message: &str) -> (String, String) {
    let (sink, mut rx) = EventSink::channel();
    run_turn(
        server,
        TurnStartParams {
            session_id: Some("test".to_string()),
            message: message.to_string(),
            stream: Some(true),
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
        // 检索门
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "delegate"}"#),
        // 父轮：派活，并要结构化结论
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
        // 子代理第一次：散文（不合规）
        Mock::text("我觉得是四吧。"),
        // 子代理第二次（带报错重试）：合规 JSON
        Mock::text(r#"{"answer": 4}"#),
        // 父轮收尾
        Mock::text("子代理说答案是 4。"),
    ])
    .await;

    let (tool_output, reply) = collect(&server, "派个子代理去算 2+2").await;

    assert!(reply.contains("4"), "{reply}");
    assert!(
        tool_output.contains("转述"),
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
        Mock::text("四。"),     // 第一次：不是 JSON
        Mock::text("还是四。"), // 第二次：仍不合规
        Mock::text("它说是四。"),
    ])
    .await;

    let (tool_output, _) = collect(&server, "派个子代理去算 2+2").await;

    assert!(
        tool_output.contains("两次都没通过校验"),
        "两次都不合规要如实说出来：{tool_output}"
    );
    assert!(
        tool_output.contains("四。"),
        "回落时保留它的原始文字（不让活白跑）：{tool_output}"
    );
}
