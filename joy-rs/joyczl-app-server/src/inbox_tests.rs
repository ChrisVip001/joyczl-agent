//! Inbox hand-off tests — the one question that matters: does a person's "handle these"
//! actually turn recorded mail into a turn, and does failure leave the mail waiting?

use std::sync::Arc;
use std::time::Duration;

use joyczl_config::Settings;
use joyczl_protocol::{
    codes, methods, InboxListView, InboxSummarizeResponse, JsonRpcMessage, JsonRpcRequest,
    RequestId,
};
use joyczl_provider::mock::Mock;
use joyczl_provider::Resolved;
use serde_json::json;
use tempfile::tempdir;
use tokio::sync::mpsc;

use crate::{EventSink, Frame, Server};

async fn boot(mock: Option<Arc<Mock>>) -> Server {
    let dir = tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    let _ = dir.keep();
    let server = Server::boot(
        pool,
        Settings {
            home,
            graph_workflows: false,
            ..Settings::default()
        },
    )
    .await;
    if let Some(mock) = mock {
        server.set_resolved(Some(Resolved {
            context_window: joyczl_provider::DEFAULT_CONTEXT_WINDOW,
            vision: false,
            provider_id: "mock".to_string(),
            client: mock,
            model: "test-model".to_string(),
            small_model: "test-small".to_string(),
        }));
    }
    server
}

async fn call(
    server: &Server,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let request = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: RequestId::Number(1),
        method: method.to_string(),
        params: Some(params),
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    crate::handle(server, request, EventSink::new(tx)).await;
    let mut response = None;
    while let Ok(frame) = rx.try_recv() {
        if let Frame::Response(message) = frame {
            response = Some(message);
        }
    }
    match response.expect("每个请求必有应答") {
        JsonRpcMessage::Response(response) => Ok(response.result),
        JsonRpcMessage::Error(error) => Err(error.error.message),
    }
}

async fn record(server: &Server, uid: i64, subject: &str, body: &str) {
    server
        .inbox
        .record(
            "email",
            "INBOX",
            uid,
            None,
            "alice@corp.com",
            subject,
            "2026-11-05T09:00:00+08:00",
            body,
        )
        .await
        .expect("记一封");
}

#[tokio::test]
async fn an_empty_inbox_answers_zero_and_starts_nothing() {
    let server = boot(None).await;
    let response: InboxSummarizeResponse = serde_json::from_value(
        call(&server, methods::INBOX_SUMMARIZE, json!({}))
            .await
            .expect("空收件箱不是错误"),
    )
    .expect("形态");
    assert_eq!(response.count, 0);
    assert!(response.message.contains("没有"), "{}", response.message);
    assert_eq!(response.session_id, "inbox", "默认进收件箱会话");
}

#[tokio::test]
async fn handling_mail_runs_a_turn_and_marks_it_seen() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text("两封邮件都不需要回复。"),
        Mock::text("两封邮件都不需要回复。"),
        Mock::text("两封邮件都不需要回复。"),
    ]));
    let server = boot(Some(mock.clone())).await;
    record(&server, 1, "服务器周报", "CPU 一切正常").await;
    record(&server, 2, "发票", "附件是本月发票").await;

    let list: InboxListView = serde_json::from_value(
        call(&server, methods::INBOX_LIST, json!({}))
            .await
            .expect("读列表"),
    )
    .expect("形态");
    assert_eq!(list.unseen, 2);
    assert_eq!(list.items.len(), 2);

    let response: InboxSummarizeResponse = serde_json::from_value(
        call(&server, methods::INBOX_SUMMARIZE, json!({}))
            .await
            .expect("交给回合"),
    )
    .expect("形态");
    assert_eq!(response.count, 2);

    // The turn runs in the background: drain until the reply lands in the chat log.
    let mut history = Vec::new();
    for _ in 0..200 {
        history = server
            .chat
            .session_history("inbox")
            .await
            .expect("读对话日志");
        if !history.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!history.is_empty(), "回合该跑完并落账");
    // `session_history` pairs (user message, assistant reply) — the prompt is the first half.
    let prompt = &history[0].0;
    assert!(prompt.contains("服务器周报"), "邮件主题要进回合：{prompt}");
    assert!(prompt.contains("发票"), "两封都要进去：{prompt}");
    assert!(prompt.contains("不要替我发信"), "边界要写进指令：{prompt}");

    // And the model saw the mail body, not just subjects.
    let received = mock.received.lock().expect("锁").clone();
    let last = received.last().expect("至少一次模型调用");
    let prompt_text = serde_json::to_string(&last.messages).expect("序列化");
    assert!(prompt_text.contains("CPU 一切正常"), "正文要带上");

    assert_eq!(
        server.inbox.unseen_count("email").await.expect("数"),
        0,
        "处理过的邮件退出等待队列"
    );
}

#[tokio::test]
async fn without_a_model_the_mail_keeps_waiting() {
    let server = boot(None).await;
    record(&server, 1, "周报", "正文").await;

    let error = call(&server, methods::INBOX_SUMMARIZE, json!({}))
        .await
        .expect_err("没模型就该失败");
    assert!(error.contains("模型还没配好"), "{error}");
    assert_eq!(
        server.inbox.unseen_count("email").await.expect("数"),
        1,
        "失败不能把邮件标成处理过"
    );
    assert!(
        server
            .chat
            .session_history("inbox")
            .await
            .expect("读")
            .is_empty(),
        "也不该留下半个回合"
    );
}

/// The panel's request goes through `handle` like any other; a bad body is a parse error,
/// not a panic.
#[tokio::test]
async fn a_bad_body_is_a_clean_invalid_params() {
    let server = boot(None).await;
    let request = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: RequestId::Number(1),
        method: methods::INBOX_SUMMARIZE.to_string(),
        params: Some(json!({"sessionId": 42})),
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    crate::handle(&server, request, EventSink::new(tx)).await;
    let mut code = None;
    while let Ok(frame) = rx.try_recv() {
        if let Frame::Response(JsonRpcMessage::Error(error)) = frame {
            code = Some(error.error.code);
        }
    }
    assert_eq!(code, Some(codes::INVALID_PARAMS));
}
