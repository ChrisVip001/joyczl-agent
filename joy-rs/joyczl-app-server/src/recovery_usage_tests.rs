use crate::{EventSink, Server};
use joyczl_provider::{ContentBlock, Message, Role};
use serde_json::json;

async fn fixture(unknown: bool) -> (tempfile::TempDir, Server, joyczl_state::CheckpointRow) {
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
    server
        .tasks
        .create(joyczl_state::NewTask {
            id: "task",
            session_id: Some("chat"),
            title: "known recovery usage",
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
        .start("run", "task", Some("chat"), Some("turn"))
        .await
        .unwrap();
    server.tasks.set_running("task", "turn").await.unwrap();
    let mut messages = vec![
        Message::user_text("Authorized work"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call".into(),
                name: "external_write".into(),
                input: json!({}),
                extra: None,
            }],
        },
    ];
    if !unknown {
        messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call".into(),
                content: "Observed result".into(),
            }],
        });
    }
    server
        .checkpoints
        .save_with_operations(
            joyczl_state::checkpoints::CheckpointWrite {
                turn_id: "turn",
                session_id: "chat",
                user_message: "Authorized work",
                messages_json: &serde_json::to_string(&messages).unwrap(),
                iterations: 1,
                usage_json: r#"{"input_tokens":10,"output_tokens":5}"#,
                run_id: Some("run"),
                round_id: Some("round"),
                round_no: Some(1),
            },
            &[joyczl_state::operations::OperationSnapshot {
                tool_use_id: "call".into(),
                tool: "external_write".into(),
                input_json: "{}".into(),
                output: (!unknown).then(|| "Observed result".into()),
            }],
        )
        .await
        .unwrap();
    let row = server.checkpoints.get("turn").await.unwrap().unwrap();
    (home, server, row)
}

#[tokio::test]
async fn unknown_outcomes_charge_known_usage_without_a_model_or_double_charging() {
    let (_home, server, row) = fixture(true).await;
    let (sink, _receiver) = EventSink::channel();
    assert!(!crate::resume::resume_turn(&server, &sink, row.clone(), None).await);
    assert!(!crate::resume::resume_turn(&server, &sink, row, None).await);
    let task = server.tasks.get("task").await.unwrap().unwrap();
    assert_eq!(task.spent_tokens, 15);
    assert_eq!(task.status, joyczl_state::TaskStatus::NeedsAttention);
    assert_eq!(
        server
            .checkpoints
            .get("turn")
            .await
            .unwrap()
            .unwrap()
            .status,
        "suspended"
    );
    assert!(server.active_turns(Some("chat")).is_empty());
}

#[tokio::test]
async fn historical_usage_charging_preserves_the_human_pause_fence() {
    let (_home, server, row) = fixture(true).await;
    server
        .tasks
        .suspend("task", Some("Human pause"))
        .await
        .unwrap();
    let (sink, _receiver) = EventSink::channel();
    crate::resume::resume_turn(&server, &sink, row, None).await;
    let task = server.tasks.get("task").await.unwrap().unwrap();
    assert_eq!(task.spent_tokens, 15);
    assert_eq!(task.status, joyczl_state::TaskStatus::Suspended);
    assert!(
        joyczl_state::human_pauses::HumanPauses::new(server.pool.clone())
            .is_paused("task")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn foreign_checkpoint_usage_cannot_charge_another_execution() {
    let (_home, server, mut row) = fixture(true).await;
    server
        .tasks
        .create(joyczl_state::NewTask {
            id: "other",
            session_id: Some("other-chat"),
            title: "unrelated",
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
        .start("other-run", "other", Some("other-chat"), Some("other-turn"))
        .await
        .unwrap();
    row.run_id = Some("other-run".into());
    let (sink, _receiver) = EventSink::channel();
    crate::resume::resume_turn(&server, &sink, row, None).await;
    assert_eq!(
        server
            .tasks
            .get("other")
            .await
            .unwrap()
            .unwrap()
            .spent_tokens,
        0
    );
    assert_eq!(
        server
            .tasks
            .get("task")
            .await
            .unwrap()
            .unwrap()
            .spent_tokens,
        0
    );
}

#[tokio::test]
async fn a_charging_failure_keeps_evidence_and_requires_attention_until_retry() {
    let (_home, server, row) = fixture(true).await;
    sqlx::query("CREATE TRIGGER fail_recovery_charge BEFORE INSERT ON run_round_accounts BEGIN SELECT RAISE(FAIL,'injected usage failure'); END").execute(&server.pool).await.unwrap();
    let (sink, _receiver) = EventSink::channel();
    crate::resume::resume_turn(&server, &sink, row.clone(), None).await;
    let task = server.tasks.get("task").await.unwrap().unwrap();
    assert_eq!(task.status, joyczl_state::TaskStatus::NeedsAttention);
    assert!(task
        .last_stop_reason
        .unwrap()
        .contains("could not be charged"));
    assert_eq!(
        server
            .checkpoints
            .get("turn")
            .await
            .unwrap()
            .unwrap()
            .usage_json,
        row.usage_json
    );
    sqlx::query("DROP TRIGGER fail_recovery_charge")
        .execute(&server.pool)
        .await
        .unwrap();
    crate::resume::resume_turn(&server, &sink, row, None).await;
    assert_eq!(
        server
            .tasks
            .get("task")
            .await
            .unwrap()
            .unwrap()
            .spent_tokens,
        15
    );
}

#[tokio::test]
async fn missing_model_configuration_does_not_hide_known_native_usage() {
    let (_home, server, row) = fixture(false).await;
    let (sink, _receiver) = EventSink::channel();
    assert!(!crate::resume::resume_turn(&server, &sink, row, None).await);
    assert_eq!(
        server
            .tasks
            .get("task")
            .await
            .unwrap()
            .unwrap()
            .spent_tokens,
        15
    );
    assert_eq!(
        server
            .checkpoints
            .get("turn")
            .await
            .unwrap()
            .unwrap()
            .status,
        "open"
    );
}
