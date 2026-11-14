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
    let dir = tempdir().expect("temporary directory");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("open the store");
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
    match response.expect("every request has a reply") {
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
        .expect("record one message");
}

#[tokio::test]
async fn an_empty_inbox_answers_zero_and_starts_nothing() {
    let server = boot(None).await;
    let response: InboxSummarizeResponse = serde_json::from_value(
        call(&server, methods::INBOX_SUMMARIZE, json!({}))
            .await
            .expect("an empty inbox is not an error"),
    )
    .expect("its shape");
    assert_eq!(response.count, 0);
    assert!(response.message.contains("waiting"), "{}", response.message);
    assert_eq!(
        response.session_id, "inbox",
        "the default is the inbox session"
    );
}

#[tokio::test]
async fn handling_mail_runs_a_turn_and_marks_it_seen() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text("neither message needs a reply."),
        Mock::text("neither message needs a reply."),
        Mock::text("neither message needs a reply."),
    ]));
    let server = boot(Some(mock.clone())).await;
    record(&server, 1, "server weekly report", "CPU all normal").await;
    record(
        &server,
        2,
        "invoice",
        "the attachment is this month's invoice",
    )
    .await;

    let list: InboxListView = serde_json::from_value(
        call(&server, methods::INBOX_LIST, json!({}))
            .await
            .expect("read the list"),
    )
    .expect("its shape");
    assert_eq!(list.unseen, 2);
    assert_eq!(list.items.len(), 2);

    let response: InboxSummarizeResponse = serde_json::from_value(
        call(&server, methods::INBOX_SUMMARIZE, json!({}))
            .await
            .expect("hand it to the turn"),
    )
    .expect("its shape");
    assert_eq!(response.count, 2);

    // The turn runs in the background: drain until the reply lands in the chat log.
    let mut history = Vec::new();
    for _ in 0..200 {
        history = server
            .chat
            .session_history("inbox")
            .await
            .expect("read the conversation log");
        if !history.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !history.is_empty(),
        "the turn should finish and reach the ledger"
    );
    // `session_history` pairs (user message, assistant reply) — the prompt is the first half.
    let prompt = &history[0].0;
    assert!(
        prompt.contains("server weekly report"),
        "the subject has to reach the turn: {prompt}"
    );
    assert!(
        prompt.contains("invoice"),
        "both messages have to go in: {prompt}"
    );
    assert!(
        prompt.contains("do not send anything on my behalf"),
        "the boundary has to be in the instruction: {prompt}"
    );

    // And the model saw the mail body, not just subjects.
    let received = mock.received.lock().expect("lock").clone();
    let last = received.last().expect("at least one model call");
    let prompt_text = serde_json::to_string(&last.messages).expect("serialize");
    assert!(
        prompt_text.contains("CPU all normal"),
        "the body has to come along"
    );

    assert_eq!(
        server.inbox.unseen_count("email").await.expect("count"),
        0,
        "a handled message leaves the waiting queue"
    );
}

#[tokio::test]
async fn without_a_model_the_mail_keeps_waiting() {
    let server = boot(None).await;
    record(&server, 1, "weekly report", "the body").await;

    let error = call(&server, methods::INBOX_SUMMARIZE, json!({}))
        .await
        .expect_err("no model means failure");
    assert!(error.contains("No model is configured"), "{error}");
    assert_eq!(
        server.inbox.unseen_count("email").await.expect("count"),
        1,
        "a failure must not mark the message handled"
    );
    assert!(
        server
            .chat
            .session_history("inbox")
            .await
            .expect("read")
            .is_empty(),
        "nor leave half a turn behind"
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
