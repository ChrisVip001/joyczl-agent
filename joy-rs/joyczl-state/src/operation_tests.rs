use crate::{
    operations::{OperationSnapshot, Reconciliation, ToolOperations},
    Checkpoints, NewTask, Tasks,
};

async fn fixture() -> (
    tempfile::TempDir,
    sqlx::SqlitePool,
    Checkpoints,
    ToolOperations,
) {
    let home = tempfile::tempdir().unwrap();
    let pool = crate::open(&home.path().join("state.db")).await.unwrap();
    Tasks::new(pool.clone())
        .create(NewTask {
            id: "task",
            session_id: Some("chat"),
            title: "work",
            goal_condition: None,
            acceptance_json: "[]",
            budget_json: "{}",
            cwd: None,
            max_rounds: 3,
            tier: None,
        })
        .await
        .unwrap();
    crate::Runs::new(pool.clone())
        .start("run", "task", Some("work"), Some("turn"))
        .await
        .unwrap();
    (
        home,
        pool.clone(),
        Checkpoints::new(pool.clone()),
        ToolOperations::new(pool),
    )
}
fn operation(output: Option<&str>) -> OperationSnapshot {
    OperationSnapshot {
        tool_use_id: "call".into(),
        tool: "external_write".into(),
        input_json: r#"{"value":42}"#.into(),
        output: output.map(str::to_string),
    }
}
async fn save(
    checkpoints: &Checkpoints,
    messages: &str,
    operations: &[OperationSnapshot],
) -> anyhow::Result<()> {
    checkpoints
        .save_with_operations(
            "turn",
            "work",
            "work",
            messages,
            0,
            "{}",
            Some("run"),
            Some("round"),
            Some(1),
            operations,
        )
        .await
}

#[tokio::test]
async fn intent_and_result_commit_with_the_exact_boundary_and_cannot_be_reused() {
    let (home, pool, checkpoints, operations) = fixture().await;
    save(&checkpoints, "[]", &[operation(None)]).await.unwrap();
    let intent = operations.list("task").await.unwrap().remove(0);
    assert_eq!(intent.status, "intent");
    let reopened = crate::open(&home.path().join("state.db")).await.unwrap();
    assert_eq!(
        ToolOperations::new(reopened)
            .get(&intent.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "intent"
    );
    save(&checkpoints, "[1]", &[operation(Some("Committed"))])
        .await
        .unwrap();
    let settled = operations.get(&intent.id).await.unwrap().unwrap();
    assert_eq!(settled.status, "succeeded");
    assert_eq!(
        settled.output_digest,
        Some(crate::ledger::digest_of(b"Committed"))
    );
    assert!(save(&checkpoints, "[2]", &[operation(None)]).await.is_err());
    let mut changed = operation(Some("Committed"));
    changed.input_json = r#"{"value":43}"#.into();
    assert!(save(&checkpoints, "[3]", &[changed]).await.is_err());
    assert_eq!(
        checkpoints
            .get("turn")
            .await
            .unwrap()
            .unwrap()
            .messages_json,
        "[1]"
    );
    sqlx::query("CREATE TRIGGER fail_operation BEFORE INSERT ON tool_operations BEGIN SELECT RAISE(FAIL,'injected operation failure'); END").execute(&pool).await.unwrap();
    let mut next = operation(None);
    next.tool_use_id = "next".into();
    assert!(save(&checkpoints, "[4]", &[next]).await.is_err());
    assert_eq!(
        checkpoints
            .get("turn")
            .await
            .unwrap()
            .unwrap()
            .messages_json,
        "[1]"
    );
}

#[tokio::test]
async fn reconciliation_is_fenced_atomic_immutable_and_does_not_resume() {
    let (_home, pool, checkpoints, operations) = fixture().await;
    save(&checkpoints, "[]", &[operation(None)]).await.unwrap();
    let intent = operations.list("task").await.unwrap().remove(0);
    Tasks::new(pool.clone())
        .suspend("task", Some("Human review"))
        .await
        .unwrap();
    checkpoints.finish("turn", "suspended").await.unwrap();
    let digest = crate::ledger::digest_of(b"[]");
    let input = Reconciliation {
        owner: crate::operations::OperationOwner::Task("task"),
        operation_id: &intent.id,
        expected_checkpoint_digest: &digest,
        resolution: "reconciled_success",
        evidence: "External receipt tx-42 verified by the user",
        messages_json: "[1]",
    };
    assert!(operations
        .reconcile(Reconciliation {
            expected_checkpoint_digest: "stale",
            ..input
        })
        .await
        .is_err());
    sqlx::query("CREATE TRIGGER fail_receipt BEFORE UPDATE OF messages_json ON turn_checkpoints BEGIN SELECT RAISE(FAIL,'injected receipt failure'); END").execute(&pool).await.unwrap();
    assert!(operations.reconcile(input.clone()).await.is_err());
    assert_eq!(
        operations.get(&intent.id).await.unwrap().unwrap().status,
        "intent"
    );
    sqlx::query("DROP TRIGGER fail_receipt")
        .execute(&pool)
        .await
        .unwrap();
    operations.reconcile(input.clone()).await.unwrap();
    operations.reconcile(input.clone()).await.unwrap();
    assert_eq!(
        operations.get(&intent.id).await.unwrap().unwrap().status,
        "reconciled_success"
    );
    assert_eq!(
        checkpoints
            .get("turn")
            .await
            .unwrap()
            .unwrap()
            .messages_json,
        "[1]"
    );
    assert_eq!(
        crate::Tasks::new(pool.clone())
            .get("task")
            .await
            .unwrap()
            .unwrap()
            .status,
        crate::TaskStatus::Suspended
    );
    assert!(crate::human_pauses::HumanPauses::new(pool)
        .is_paused("task")
        .await
        .unwrap());
    assert!(operations
        .reconcile(Reconciliation {
            resolution: "not_executed",
            evidence: "Different claim",
            ..input
        })
        .await
        .is_err());
}
