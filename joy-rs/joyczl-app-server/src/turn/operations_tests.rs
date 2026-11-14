use crate::{EventSink, Server};
use joyczl_provider::{mock::Mock, ContentBlock, Message, Resolved, Role};
use serde_json::json;
use std::sync::Arc;

async fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Server) {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let pool = joyczl_state::open(&home.path().join("state.db"))
        .await
        .unwrap();
    let server = Server::boot(
        pool,
        joyczl_config::Settings {
            home: home.path().into(),
            provider: "test-unconfigured".into(),
            write_enabled: true,
            approval: "never".into(),
            ..Default::default()
        },
    )
    .await;
    let input = json!({"recipient":"synthetic","text":"one authorized effect"});
    let messages = vec![
        Message::user_text("Perform the authorized action"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "external-call".into(),
                name: "external_write".into(),
                input: input.clone(),
                extra: None,
            }],
        },
    ];
    server
        .checkpoints
        .save_with_operations(
            joyczl_state::checkpoints::CheckpointWrite {
                turn_id: "unknown-old",
                session_id: "chat",
                user_message: "Perform the authorized action",
                messages_json: &serde_json::to_string(&messages).unwrap(),
                iterations: 1,
                usage_json: "{}",
                run_id: None,
                round_id: Some("old-round"),
                round_no: Some(1),
            },
            &[joyczl_state::operations::OperationSnapshot {
                tool_use_id: "external-call".into(),
                tool: "external_write".into(),
                input_json: input.to_string(),
                output: None,
            }],
        )
        .await
        .unwrap();
    server
        .checkpoints
        .finish("unknown-old", "suspended")
        .await
        .unwrap();
    (home, workspace, server)
}

#[tokio::test]
async fn ordinary_chat_has_a_human_inspection_path_for_unknown_operations() {
    let (_home, _workspace, server) = fixture().await;
    let response = crate::input_wake_tests::rpc(
        &server,
        "turn/operations",
        json!({"turnId":"unknown-old","sessionId":"chat"}),
    )
    .await
    .unwrap();
    assert_eq!(response["operations"][0]["status"], "intent");
    assert!(response["checkpointDigest"].as_str().is_some());
}

#[tokio::test]
async fn unknown_ordinary_chat_effects_cannot_be_bypassed_by_a_new_turn() {
    let (_home, workspace, server) = fixture().await;
    server.install_provider(Resolved {
        client: Arc::new(Mock::new(vec![
            Mock::text(r#"{"retrieve":false,"query":"","reason":"test"}"#),
            Mock::tool_use(
                "new-write",
                "write_file",
                json!({"path":"result.txt","content":"must be blocked"}),
            ),
            Mock::text("I will wait for review."),
        ])),
        provider_id: "mock".into(),
        model: "test".into(),
        small_model: "test".into(),
        vision: false,
        context_window: 100_000,
    });
    let (sink, _receiver) = EventSink::channel();
    crate::run_turn(
        &server,
        joyczl_protocol::TurnStartParams {
            session_id: Some("chat".into()),
            message: "Continue the work".into(),
            cwd: Some(workspace.path().display().to_string()),
            ..Default::default()
        },
        joyczl_protocol::RequestId::Number(1),
        &sink,
    )
    .await
    .unwrap();
    assert!(
        !workspace.path().join("result.txt").exists(),
        "An unresolved earlier effect must block new mutations, while allowing chat"
    );
}

#[tokio::test]
async fn chat_reconciliation_is_scoped_atomic_idempotent_and_never_restarts() {
    let (_home, workspace, server) = fixture().await;
    let call = |method, params| crate::input_wake_tests::rpc(&server, method, params);
    let inspected = call(
        "turn/operations",
        json!({"turnId":"unknown-old","sessionId":"chat"}),
    )
    .await
    .unwrap();
    let original = server
        .checkpoints
        .get("unknown-old")
        .await
        .unwrap()
        .unwrap();
    let params = json!({"turnId":"unknown-old","sessionId":"chat","operationId":inspected["operations"][0]["id"],"checkpointDigest":inspected["checkpointDigest"],"resolution":"reconciled_success","evidence":"User checked synthetic receipt tx-1"});
    let mut foreign = params.clone();
    foreign["sessionId"] = json!("other");
    assert!(call("turn/reconcile", foreign).await.is_err());
    let mut stale = params.clone();
    stale["checkpointDigest"] = json!("stale");
    assert!(call("turn/reconcile", stale).await.is_err());
    sqlx::query("CREATE TRIGGER fail_chat_receipt BEFORE UPDATE OF messages_json ON turn_checkpoints BEGIN SELECT RAISE(FAIL,'injected chat receipt failure'); END").execute(&server.pool).await.unwrap();
    assert!(call("turn/reconcile", params.clone()).await.is_err());
    assert_eq!(
        server
            .checkpoints
            .get("unknown-old")
            .await
            .unwrap()
            .unwrap()
            .messages_json,
        original.messages_json
    );
    let id = inspected["operations"][0]["id"].as_str().unwrap();
    let operations = joyczl_state::operations::ToolOperations::new(server.pool.clone());
    assert_eq!(operations.get(id).await.unwrap().unwrap().status, "intent");
    sqlx::query("DROP TRIGGER fail_chat_receipt")
        .execute(&server.pool)
        .await
        .unwrap();
    let settled = call("turn/reconcile", params.clone()).await.unwrap();
    assert_eq!(settled["operations"][0]["status"], "reconciled_success");
    assert!(call("turn/reconcile", params.clone()).await.is_ok());
    let mut conflict = params;
    conflict["evidence"] = json!("A conflicting claim");
    assert!(call("turn/reconcile", conflict).await.is_err());
    let cp = server
        .checkpoints
        .get("unknown-old")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cp.status, "suspended");
    assert!(cp.messages_json.contains("Human reconciliation:"));
    assert!(server.active_turns(Some("chat")).is_empty());
    let context = crate::turn::operations::context(&server, "chat")
        .await
        .unwrap();
    assert!(!context.unresolved);
    assert!(context.prompt.contains("tx-1"));
    server.install_provider(Resolved {
        client: Arc::new(Mock::new(vec![
            Mock::text(r#"{"retrieve":false,"query":"","reason":"test"}"#),
            Mock::tool_use(
                "authorized-new-write",
                "write_file",
                json!({"path":"new.txt","content":"explicit new instruction"}),
            ),
            Mock::text("New work completed."),
        ])),
        provider_id: "mock".into(),
        model: "test".into(),
        small_model: "test".into(),
        vision: false,
        context_window: 100_000,
    });
    let (sink, _receiver) = EventSink::channel();
    crate::run_turn(
        &server,
        joyczl_protocol::TurnStartParams {
            session_id: Some("chat".into()),
            message: "The old effect is verified. Write a separate new file.".into(),
            cwd: Some(workspace.path().display().to_string()),
            ..Default::default()
        },
        joyczl_protocol::RequestId::Number(2),
        &sink,
    )
    .await
    .unwrap();
    assert!(workspace.path().join("new.txt").exists());
    assert_eq!(
        operations.get(id).await.unwrap().unwrap().status,
        "reconciled_success"
    );
}

#[tokio::test]
async fn legacy_chat_intents_are_recovered_without_restarting_or_broadening_scope() {
    let (_home, _workspace, server) = fixture().await;
    sqlx::query("DELETE FROM tool_operations WHERE turn_id='unknown-old'")
        .execute(&server.pool)
        .await
        .unwrap();
    assert!(
        crate::turn::operations::context(&server, "chat")
            .await
            .unwrap()
            .unresolved
    );
    let inspected =
        crate::input_wake_tests::rpc(&server, "turn/operations", json!({"sessionId":"chat"}))
            .await
            .unwrap();
    assert_eq!(inspected["operations"][0]["origin"], "recovered_checkpoint");
    assert!(inspected["operations"][0]["checkpointDigest"].is_string());
    assert_eq!(
        server
            .checkpoints
            .get("unknown-old")
            .await
            .unwrap()
            .unwrap()
            .status,
        "suspended"
    );
}

#[tokio::test]
async fn unclaimed_open_chat_uncertainty_is_not_a_startup_bypass() {
    let (_home, _workspace, server) = fixture().await;
    server
        .checkpoints
        .finish("unknown-old", "open")
        .await
        .unwrap();
    assert!(
        crate::turn::operations::context(&server, "chat")
            .await
            .unwrap()
            .unresolved
    );
    let inspected =
        crate::input_wake_tests::rpc(&server, "turn/operations", json!({"sessionId":"chat"}))
            .await
            .unwrap();
    assert_eq!(inspected["operations"][0]["status"], "intent");
    assert_eq!(
        server
            .checkpoints
            .get("unknown-old")
            .await
            .unwrap()
            .unwrap()
            .status,
        "suspended"
    );
}
