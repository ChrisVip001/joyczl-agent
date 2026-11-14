//! The todo list's **place within a turn**: it is injected into the system prompt, so compaction and sliding windows cannot wash it away.

use std::sync::Arc;

use joyczl_config::Settings;
use joyczl_protocol::{RequestId, TurnStartParams};
use joyczl_provider::mock::Mock;
use joyczl_provider::Resolved;
use serde_json::json;

use crate::{EventSink, Server};

/// Run two turns on the same server: the board is **session-level**, so it must live on the server, not in any one turn.
async fn run_twice(
    first_message: &str,
    second_message: &str,
) -> (Arc<Mock>, String, String, String) {
    let dir = tempfile::tempdir().expect("temporary directory");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("open the store");
    let _ = dir.keep();

    let mock = Arc::new(Mock::new(vec![
        // First turn: retrieval gate → write the board → wind down
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "todo"}"#),
        Mock::tool_use(
            "call-1",
            "todo_write",
            json!({"todos": [
                {"content": "write tests", "status": "in_progress"},
                {"content": "update the docs", "status": "pending"}
            ]}),
        ),
        Mock::text("the list is noted."),
        // Second turn: retrieval gate → wind down
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "todo"}"#),
        Mock::text("carrying on."),
    ]));

    let server = Server::boot(
        pool,
        Settings {
            home: home.clone(),
            ..Settings::default()
        },
    )
    .await;
    server.set_resolved(Some(Resolved {
        context_window: joyczl_provider::DEFAULT_CONTEXT_WINDOW,
        vision: false,
        provider_id: "mock".to_string(),
        client: mock.clone(),
        model: "test-model".to_string(),
        small_model: "test-small".to_string(),
    }));

    let mut tools_first = String::new();
    let mut system_second = String::new();
    let mut reply_second = String::new();
    for (index, message) in [first_message, second_message].into_iter().enumerate() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = EventSink::new(tx);
        crate::turn::run_turn(
            &server,
            TurnStartParams {
                attachments: None,
                session_id: Some("todo-session".to_string()),
                message: message.to_string(),
                stream: Some(true),
                cwd: None,
                tier: None,
            },
            RequestId::Number(index as i32 + 1),
            &sink,
        )
        .await
        .expect("this turn should finish");

        while let Ok(frame) = rx.try_recv() {
            match frame {
                crate::Frame::Notification(joyczl_protocol::ServerNotification::ToolCompleted(
                    done,
                )) if index == 0 => {
                    tools_first = done.output.clone();
                }
                crate::Frame::Notification(joyczl_protocol::ServerNotification::TurnCompleted(
                    done,
                )) if index == 1 => {
                    reply_second = done.reply;
                }
                _ => {}
            }
        }
    }

    // The last request is the second turn's loop call (the earlier ones are short requests like the gate) —
    // no fixed index, so the test does not go falsely red when "does the gate count as a request" changes.
    let requests = mock.received.lock().unwrap();
    if let Some(last) = requests.last() {
        system_second = last.system.clone().unwrap_or_default();
    }
    drop(requests);

    (mock, tools_first, system_second, reply_second)
}

/// The board is **visible on the very next turn** after being written, and it is injected (not via history).
#[tokio::test]
async fn a_todo_list_is_injected_into_the_next_turn() {
    let (_mock, tool_output, system, reply) =
        run_twice("list what needs doing", "keep going").await;

    assert!(
        tool_output.contains("in_progress: write tests"),
        "the tool reply has to carry the authoritative list: {tool_output}"
    );
    assert!(
        system.contains(joyczl_tools::todo::INJECTION_HEADER),
        "the second round's system prompt has to carry that list section: {system}"
    );
    assert!(system.contains("in_progress: write tests"), "{system}");
    assert!(system.contains("pending: update the docs"), "{system}");

    // The model finished this turn on its own (it did not run out of steps), and two open items
    // do **not** hijack the ending: "make a plan" is done once the plan is stated, and carrying
    // on would be deciding to execute on the human's behalf. Only a **truncated** turn resumes.
    assert!(
        !reply.contains("automatic continuation"),
        "a turn that ended naturally must not be taken over by a continuation: {reply}"
    );
}

/// A session that never wrote a board gets no section (an empty header is worse than none: it would suggest the board is empty).
#[tokio::test]
async fn a_session_without_a_list_gets_no_section() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("open the store");
    let _ = dir.keep();
    let mock = Arc::new(Mock::new(vec![
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "plain"}"#),
        Mock::text("fine."),
    ]));
    let server = Server::boot(
        pool,
        Settings {
            home: home.clone(),
            ..Settings::default()
        },
    )
    .await;
    server.set_resolved(Some(Resolved {
        context_window: joyczl_provider::DEFAULT_CONTEXT_WINDOW,
        vision: false,
        provider_id: "mock".to_string(),
        client: mock.clone(),
        model: "test-model".to_string(),
        small_model: "test-small".to_string(),
    }));

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    crate::turn::run_turn(
        &server,
        TurnStartParams {
            attachments: None,
            session_id: Some("clean".to_string()),
            message: "say something, anything".to_string(),
            stream: Some(true),
            cwd: None,
            tier: None,
        },
        RequestId::Number(1),
        &EventSink::new(tx),
    )
    .await
    .expect("it finishes");

    let requests = mock.received.lock().unwrap();
    let system = requests[0].system.clone().unwrap_or_default();
    assert!(
        !system.contains(joyczl_tools::todo::INJECTION_HEADER),
        "no list means no such section: {system}"
    );
}
