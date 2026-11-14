use crate::{EventSink, Frame, Server};
use joyczl_protocol::{JsonRpcMessage, JsonRpcRequest, RequestId, TurnStartParams};
use joyczl_provider::{mock::Mock, Message, Resolved};
use serde_json::json;
use std::sync::Arc;

async fn fixture(
    unknown: bool,
    replies: Vec<joyczl_provider::CreateResponse>,
) -> (tempfile::TempDir, Server, Arc<Mock>) {
    let home = tempfile::tempdir().unwrap();
    let pool = joyczl_state::open(&home.path().join("state.db"))
        .await
        .unwrap();
    let server = Server::boot(
        pool,
        joyczl_config::Settings {
            home: home.path().into(),
            ..Default::default()
        },
    )
    .await;
    let mock = Arc::new(Mock::new(replies));
    server.install_provider(Resolved {
        client: mock.clone(),
        model: "test".into(),
        small_model: "test".into(),
        provider_id: "mock".into(),
        vision: false,
        context_window: 100_000,
    });
    server
        .tasks
        .create(joyczl_state::NewTask {
            id: "task",
            session_id: Some("chat"),
            title: "work",
            goal_condition: None,
            acceptance_json: "[]",
            budget_json: "{}",
            cwd: None,
            max_rounds: 1,
            tier: None,
        })
        .await
        .unwrap();
    server
        .runs
        .start("run", "task", Some("chat"), Some("run"))
        .await
        .unwrap();
    let mut messages = vec![Message::user_text("authorized work")];
    if unknown {
        messages.push(Message {
            role: joyczl_provider::Role::Assistant,
            content: vec![joyczl_provider::ContentBlock::ToolUse {
                id: "unknown-call".into(),
                name: "write_file".into(),
                input: json!({"path":"result.txt","content":"commit"}),
                extra: None,
            }],
        });
    }
    server
        .checkpoints
        .save_round(
            "run",
            "chat",
            "authorized work",
            &serde_json::to_string(&messages).unwrap(),
            0,
            "{}",
            Some("run"),
            Some("round"),
        )
        .await
        .unwrap();
    server
        .checkpoints
        .finish("run", if unknown { "suspended" } else { "done" })
        .await
        .unwrap();
    server
        .tasks
        .needs_attention("task", "Review required")
        .await
        .unwrap();
    server
        .runs
        .close(
            "run",
            joyczl_state::RunStatus::Waiting,
            Some("Review required"),
        )
        .await
        .unwrap();
    (home, server, mock)
}

#[tokio::test]
async fn attention_without_heartbeat_cannot_resume_an_unknown_tool_intent() {
    let (_home, server, mock) = fixture(true, vec![]).await;
    let (sink, mut receiver) = EventSink::channel();
    crate::handle(
        &server,
        JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: RequestId::Number(1),
            method: "task/resume".into(),
            params: Some(json!({"taskId":"task"})),
        },
        sink,
    )
    .await;
    let mut rejected = false;
    while let Ok(frame) = receiver.try_recv() {
        if let Frame::Response(JsonRpcMessage::Error(error)) = frame {
            rejected = error.error.message.contains("reconcile");
        }
    }
    assert!(
        rejected,
        "Human resume must first reconcile an unknown operation"
    );
    assert_eq!(
        server.tasks.get("task").await.unwrap().unwrap().status,
        joyczl_state::TaskStatus::NeedsAttention
    );
    assert!(mock.received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_new_chat_turn_cannot_bypass_an_unknown_task_checkpoint() {
    let (_home, server, mock) = fixture(true, vec![]).await;
    let (sink, _receiver) = EventSink::channel();
    let error = crate::run_turn(
        &server,
        TurnStartParams {
            session_id: Some("chat".into()),
            message: "continue".into(),
            ..Default::default()
        },
        RequestId::Number(1),
        &sink,
    )
    .await
    .unwrap_err();
    assert!(error.message.contains("reconcile"), "{}", error.message);
    assert_eq!(server.runs.list_by_task("task", 10).await.unwrap().len(), 1);
    assert!(mock.received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn known_attention_checkpoint_resumes_its_exact_run_without_a_heartbeat() {
    let (_home, server, mock) = fixture(false, vec![Mock::text("Produced.")]).await;
    let (sink, mut receiver) = EventSink::channel();
    crate::handle(
        &server,
        JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: RequestId::Number(1),
            method: "task/resume".into(),
            params: Some(json!({"taskId":"task"})),
        },
        sink,
    )
    .await;
    while let Ok(frame) = receiver.try_recv() {
        if let Frame::Response(JsonRpcMessage::Error(error)) = frame {
            panic!("{}", error.error.message);
        }
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if server.tasks.get("task").await.unwrap().unwrap().status
                == joyczl_state::TaskStatus::Done
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(server.runs.list_by_task("task", 10).await.unwrap().len(), 1);
    assert_eq!(mock.received.lock().unwrap().len(), 1);
    assert_eq!(
        server.runs.get("run").await.unwrap().unwrap().status,
        "completed"
    );
}

#[tokio::test]
async fn exhausted_budget_blocks_both_new_turns_and_checkpoint_recovery() {
    for checkpoint_tokens in [0_i64, 12] {
        let (_home, server, mock) = fixture(false, vec![Mock::text("must not run")]).await;
        let already_spent = if checkpoint_tokens == 0 { 10 } else { 0 };
        sqlx::query(
            "UPDATE agent_tasks SET budget_json='{\"tokens\":10}',spent_tokens=? WHERE id='task'",
        )
        .bind(already_spent)
        .execute(&server.pool)
        .await
        .unwrap();
        if checkpoint_tokens == 0 {
            let (sink, _receiver) = EventSink::channel();
            assert!(crate::run_turn(
                &server,
                TurnStartParams {
                    session_id: Some("chat".into()),
                    message: "Continue the task".into(),
                    ..Default::default()
                },
                RequestId::Number(1),
                &sink
            )
            .await
            .is_err());
        }
        sqlx::query("UPDATE turn_checkpoints SET status='open',usage_json=? WHERE turn_id='run'")
            .bind(format!(
                "{{\"input_tokens\":{checkpoint_tokens},\"output_tokens\":0}}"
            ))
            .execute(&server.pool)
            .await
            .unwrap();
        let (sink, _receiver) = EventSink::channel();
        crate::resume::reconcile(&server, &sink).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if server.checkpoints.get("run").await.unwrap().unwrap().status == "suspended" {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(mock.received.lock().unwrap().is_empty());
        let task = server.tasks.get("task").await.unwrap().unwrap();
        assert_eq!(task.status, joyczl_state::TaskStatus::NeedsAttention);
        assert_eq!(task.spent_tokens, already_spent.max(checkpoint_tokens));
        assert!(task.last_stop_reason.unwrap().contains("budget"));
    }
}

#[tokio::test]
async fn corrupt_checkpoint_usage_leaves_attention_instead_of_a_running_task() {
    let (_home, server, mock) = fixture(false, vec![]).await;
    server.tasks.set_running("task", "run").await.unwrap();
    sqlx::query("UPDATE turn_checkpoints SET status='open',usage_json='{\"input_tokens\":-1}' WHERE turn_id='run'")
        .execute(&server.pool).await.unwrap();
    let (sink, _receiver) = EventSink::channel();
    crate::resume::reconcile(&server, &sink).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if server.checkpoints.get("run").await.unwrap().unwrap().status == "suspended" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        server.tasks.get("task").await.unwrap().unwrap().status,
        joyczl_state::TaskStatus::NeedsAttention
    );
    assert!(mock.received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn late_turn_failures_preserve_human_stops() {
    for (status, run_status) in [
        (joyczl_state::TaskStatus::Cancelled, "cancelled"),
        (joyczl_state::TaskStatus::Suspended, "suspended"),
        (joyczl_state::TaskStatus::NeedsAttention, "waiting"),
    ] {
        let (_home, server, _mock) = fixture(false, vec![]).await;
        match status {
            joyczl_state::TaskStatus::Cancelled => {
                server.tasks.finish("task", status).await.unwrap()
            }
            joyczl_state::TaskStatus::Suspended => server
                .tasks
                .suspend("task", Some("Human pause"))
                .await
                .unwrap(),
            _ => server
                .tasks
                .needs_attention("task", "Human review")
                .await
                .unwrap(),
        }
        let (sink, _receiver) = EventSink::channel();
        crate::turn::fail_turn(
            &server,
            &sink,
            "run",
            Some("run"),
            Some("task"),
            "Late provider error",
        )
        .await;
        assert_eq!(
            server.tasks.get("task").await.unwrap().unwrap().status,
            status
        );
        assert_eq!(
            server.runs.get("run").await.unwrap().unwrap().status,
            run_status
        );
    }
}

#[tokio::test]
async fn an_open_checkpoint_cannot_override_a_persisted_human_pause() {
    let (_home, server, model) = fixture(false, vec![Mock::text("must not run")]).await;
    server
        .tasks
        .suspend("task", Some("Human pause"))
        .await
        .unwrap();
    server.checkpoints.reopen("run").await.unwrap();
    let (sink, _receiver) = EventSink::channel();
    crate::resume::reconcile(&server, &sink).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let task = server.tasks.get("task").await.unwrap().unwrap();
            let cp = server.checkpoints.get("run").await.unwrap().unwrap();
            if cp.status == "suspended" || task.status == joyczl_state::TaskStatus::Done {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(model.received.lock().unwrap().is_empty());
    assert_eq!(
        server.tasks.get("task").await.unwrap().unwrap().status,
        joyczl_state::TaskStatus::Suspended
    );
    assert_eq!(
        server.checkpoints.get("run").await.unwrap().unwrap().status,
        "suspended"
    );
}

#[tokio::test]
async fn recovery_errors_do_not_resurrect_a_cancelled_task() {
    let (_home, server, _model) = fixture(false, vec![]).await;
    server
        .tasks
        .finish("task", joyczl_state::TaskStatus::Cancelled)
        .await
        .unwrap();
    server
        .runs
        .close(
            "run",
            joyczl_state::RunStatus::Cancelled,
            Some("Human cancellation"),
        )
        .await
        .unwrap();
    server.checkpoints.finish("run", "abandoned").await.unwrap();
    let row = server.checkpoints.get("run").await.unwrap().unwrap();
    let (sink, _receiver) = EventSink::channel();
    crate::task::contract::stop_recovery(&server, &sink, &row, "Corrupt checkpoint usage").await;
    assert_eq!(
        server.tasks.get("task").await.unwrap().unwrap().status,
        joyczl_state::TaskStatus::Cancelled
    );
    assert_eq!(
        server.runs.get("run").await.unwrap().unwrap().status,
        "cancelled"
    );
    assert_eq!(
        server.checkpoints.get("run").await.unwrap().unwrap().status,
        "abandoned"
    );
}

#[tokio::test]
async fn an_old_checkpoint_cannot_replace_a_newer_task_execution() {
    let (_home, server, model) = fixture(false, vec![Mock::text("must not run")]).await;
    server
        .runs
        .start("new-run", "task", Some("chat"), Some("new-turn"))
        .await
        .unwrap();
    server.tasks.set_running("task", "new-turn").await.unwrap();
    server.checkpoints.reopen("run").await.unwrap();
    let (sink, _receiver) = EventSink::channel();
    crate::resume::reconcile(&server, &sink).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if server.checkpoints.get("run").await.unwrap().unwrap().status == "abandoned"
                || !model.received.lock().unwrap().is_empty()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(model.received.lock().unwrap().is_empty());
    assert_eq!(
        server
            .tasks
            .get("task")
            .await
            .unwrap()
            .unwrap()
            .current_turn_id
            .as_deref(),
        Some("new-turn")
    );
    assert_eq!(
        server.runs.get("new-run").await.unwrap().unwrap().status,
        "running"
    );
}

#[tokio::test]
async fn human_continue_waits_for_the_previous_actor_before_reusing_its_turn() {
    let (_home, server, model) = fixture(false, vec![Mock::text("Produced.")]).await;
    let _actor = server.begin_turn("run", "chat").unwrap();
    let (sink, _receiver) = EventSink::channel();
    crate::task::suspend(
        &server,
        &sink,
        joyczl_protocol::TaskActionParams {
            task_id: "task".into(),
            reason: Some("Human pause".into()),
        },
    )
    .await
    .unwrap();
    let host = server.clone();
    let request = tokio::spawn(async move {
        crate::task::resume(
            &host,
            &sink,
            joyczl_protocol::TaskActionParams {
                task_id: "task".into(),
                reason: None,
            },
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        server.tasks.get("task").await.unwrap().unwrap().status,
        joyczl_state::TaskStatus::Suspended
    );
    assert!(
        joyczl_state::human_pauses::HumanPauses::new(server.pool.clone())
            .is_paused("task")
            .await
            .unwrap()
    );
    server.finish_turn("run");
    request.await.unwrap().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if server.tasks.get("task").await.unwrap().unwrap().status
                == joyczl_state::TaskStatus::Done
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(model.received.lock().unwrap().len(), 1);
}

#[test]
fn call_ids_are_paired_chronologically_across_completed_rounds() {
    use joyczl_provider::{ContentBlock, Role};
    let call = |value| Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "call".into(),
            name: "external_write".into(),
            input: json!({"value":value}),
            extra: None,
        }],
    };
    let messages = vec![
        call(1),
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call".into(),
                content: "First round finished".into(),
            }],
        },
        call(2),
    ];
    let pending = crate::task::recovery::pending_tools(&messages).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending["call"].1["value"], 2);
    assert!(crate::task::recovery::pending_tools(&[call(1), call(2)]).is_err());
}
