use crate::{EventSink, Server};
use joyczl_provider::{mock::Mock, ContentBlock, Message, Resolved, Role};
use serde_json::json;
use std::sync::Arc;

async fn fixture() -> (tempfile::TempDir, Server, Arc<Mock>) {
    let home = tempfile::tempdir().unwrap();
    let pool = joyczl_state::open(&home.path().join("state.db"))
        .await
        .unwrap();
    let server = Server::boot(
        pool,
        joyczl_config::Settings {
            home: home.path().into(),
            provider: "test-unconfigured".into(),
            ..Default::default()
        },
    )
    .await;
    let model = Arc::new(Mock::new(vec![Mock::text(
        "The confirmed effect is retained. Produced.",
    )]));
    server.install_provider(Resolved {
        client: model.clone(),
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
            title: "transaction",
            goal_condition: None,
            acceptance_json: "[]",
            budget_json: "{}",
            cwd: None,
            max_rounds: 3,
            tier: None,
        })
        .await
        .unwrap();
    server
        .runs
        .start("run", "task", Some("work"), Some("turn"))
        .await
        .unwrap();
    let messages = vec![
        Message::user_text("Authorized transaction"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "external-call".into(),
                name: "external_write".into(),
                input: json!({"value":42}),
                extra: None,
            }],
        },
    ];
    server
        .checkpoints
        .save_with_operations(
            "turn",
            "work",
            "Authorized transaction",
            &serde_json::to_string(&messages).unwrap(),
            0,
            "{}",
            Some("run"),
            Some("round"),
            Some(1),
            &[joyczl_state::operations::OperationSnapshot {
                tool_use_id: "external-call".into(),
                tool: "external_write".into(),
                input_json: json!({"value":42}).to_string(),
                output: None,
            }],
        )
        .await
        .unwrap();
    server
        .checkpoints
        .finish("turn", "suspended")
        .await
        .unwrap();
    server
        .tasks
        .needs_attention("task", "External result unknown")
        .await
        .unwrap();
    (home, server, model)
}

#[tokio::test]
async fn a_human_receipt_resolves_only_its_operation_and_requires_explicit_continue() {
    let (home, server, model) = fixture().await;
    std::fs::write(home.path().join("external-receipt"), "tx-42: one effect").unwrap();
    let original = crate::task_operations::inspect(&server, "task")
        .await
        .unwrap();
    assert_eq!(original.operations.len(), 1);
    assert_eq!(original.operations[0].replay_policy, "never");
    let params = joyczl_protocol::TaskReconcileParams {
        task_id: "task".into(),
        operation_id: original.operations[0].id.clone(),
        checkpoint_digest: original.checkpoint_digest.unwrap(),
        resolution: "reconciled_success".into(),
        evidence: "Verified external receipt tx-42; exactly one effect".into(),
    };
    let (sink, _receiver) = EventSink::channel();
    let mut stale = params.clone();
    stale.checkpoint_digest = "stale".into();
    assert!(crate::task_operations::reconcile(&server, &sink, stale)
        .await
        .is_err());
    let _actor = server.begin_turn("turn", "work").unwrap();
    assert!(
        crate::task_operations::reconcile(&server, &sink, params.clone())
            .await
            .is_err()
    );
    server.finish_turn("turn");
    let settled = crate::task_operations::reconcile(&server, &sink, params.clone())
        .await
        .unwrap();
    assert_eq!(settled.operations[0].status, "reconciled_success");
    crate::task_operations::reconcile(&server, &sink, params)
        .await
        .unwrap();
    assert!(model.received.lock().unwrap().is_empty());
    assert_eq!(
        server.tasks.get("task").await.unwrap().unwrap().status,
        joyczl_state::TaskStatus::NeedsAttention
    );
    let cp = server.checkpoints.get("turn").await.unwrap().unwrap();
    crate::task_recovery::require_known(&cp).unwrap();
    assert!(cp.messages_json.contains("Human reconciliation:"));
    crate::task::resume(
        &server,
        &sink,
        joyczl_protocol::TaskActionParams {
            task_id: "task".into(),
            reason: None,
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
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
    assert_eq!(
        std::fs::read_to_string(home.path().join("external-receipt")).unwrap(),
        "tx-42: one effect"
    );
    assert_eq!(model.received.lock().unwrap().len(), 1);
    assert_eq!(
        server
            .steps
            .count_ok_by_tool_in_run("task", "run", "external_write")
            .await
            .unwrap(),
        0
    );
    assert_eq!(server.runs.count_by_task("task").await.unwrap(), 1);
}

#[tokio::test]
async fn stopped_legacy_checkpoints_recover_only_labelled_operation_metadata() {
    let (_home, server, model) = fixture().await;
    sqlx::query("DELETE FROM tool_operations")
        .execute(&server.pool)
        .await
        .unwrap();
    let before = server.checkpoints.get("turn").await.unwrap().unwrap();
    let view = crate::task_operations::inspect(&server, "task")
        .await
        .unwrap();
    assert_eq!(view.operations.len(), 1);
    assert_eq!(view.operations[0].origin, "recovered_checkpoint");
    assert_eq!(view.operations[0].status, "intent");
    let after = server.checkpoints.get("turn").await.unwrap().unwrap();
    assert_eq!(before.messages_json, after.messages_json);
    assert_eq!(after.status, "suspended");
    assert!(model.received.lock().unwrap().is_empty());
    let (sink, _receiver) = EventSink::channel();
    crate::task_operations::reconcile(
        &server,
        &sink,
        joyczl_protocol::TaskReconcileParams {
            task_id: "task".into(),
            operation_id: view.operations[0].id.clone(),
            checkpoint_digest: view.checkpoint_digest.unwrap(),
            resolution: "not_executed".into(),
            evidence: "Checked external service; no transaction exists".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        server.tasks.get("task").await.unwrap().unwrap().status,
        joyczl_state::TaskStatus::NeedsAttention
    );
}
