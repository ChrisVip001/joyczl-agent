use crate::{Checkpoints, NewTask, Runs, Tasks};

#[tokio::test]
async fn checkpoint_admission_records_the_round_before_work_can_start() {
    let home = tempfile::tempdir().unwrap();
    let pool = crate::open(&home.path().join("state.db")).await.unwrap();
    let tasks = Tasks::new(pool.clone());
    tasks
        .create(NewTask {
            id: "task",
            session_id: Some("chat"),
            title: "work",
            goal_condition: Some("finish work"),
            acceptance_json: "[]",
            budget_json: "{}",
            cwd: None,
            max_rounds: 3,
            tier: None,
        })
        .await
        .unwrap();
    Runs::new(pool.clone())
        .start("run", "task", Some("chat"), Some("turn"))
        .await
        .unwrap();
    let checkpoints = Checkpoints::new(pool.clone());
    checkpoints
        .save_numbered_round(
            "turn",
            "chat",
            "work",
            "[]",
            0,
            "{}",
            Some("run"),
            Some("first"),
            Some(1),
        )
        .await
        .unwrap();
    assert_eq!(tasks.get("task").await.unwrap().unwrap().rounds_used, 1);
    checkpoints
        .save_numbered_round(
            "turn",
            "chat",
            "next",
            "[2]",
            0,
            "{}",
            Some("run"),
            Some("second"),
            Some(2),
        )
        .await
        .unwrap();
    assert_eq!(tasks.get("task").await.unwrap().unwrap().rounds_used, 2);
    assert_eq!(
        checkpoints.get("turn").await.unwrap().unwrap().round_no,
        Some(2)
    );
    sqlx::query("CREATE TRIGGER fail_round BEFORE UPDATE OF rounds_used ON agent_tasks BEGIN SELECT RAISE(FAIL,'injected round failure'); END").execute(&pool).await.unwrap();
    assert!(checkpoints
        .save_numbered_round(
            "turn",
            "chat",
            "work",
            "[3]",
            1,
            "{}",
            Some("run"),
            Some("first"),
            Some(1),
        )
        .await
        .is_err());
    assert_eq!(
        checkpoints
            .get("turn")
            .await
            .unwrap()
            .unwrap()
            .messages_json,
        "[2]"
    );
}
