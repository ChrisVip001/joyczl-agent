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
        .save_numbered_round(crate::checkpoints::CheckpointWrite {
            turn_id: "turn",
            session_id: "chat",
            user_message: "work",
            messages_json: "[]",
            iterations: 0,
            usage_json: "{}",
            run_id: Some("run"),
            round_id: Some("first"),
            round_no: Some(1),
        })
        .await
        .unwrap();
    assert_eq!(tasks.get("task").await.unwrap().unwrap().rounds_used, 1);
    checkpoints
        .save_numbered_round(crate::checkpoints::CheckpointWrite {
            turn_id: "turn",
            session_id: "chat",
            user_message: "next",
            messages_json: "[2]",
            iterations: 0,
            usage_json: "{}",
            run_id: Some("run"),
            round_id: Some("second"),
            round_no: Some(2),
        })
        .await
        .unwrap();
    assert_eq!(tasks.get("task").await.unwrap().unwrap().rounds_used, 2);
    assert_eq!(
        checkpoints.get("turn").await.unwrap().unwrap().round_no,
        Some(2)
    );
    sqlx::query("CREATE TRIGGER fail_round BEFORE UPDATE OF rounds_used ON agent_tasks BEGIN SELECT RAISE(FAIL,'injected round failure'); END").execute(&pool).await.unwrap();
    assert!(checkpoints
        .save_numbered_round(crate::checkpoints::CheckpointWrite {
            turn_id: "turn",
            session_id: "chat",
            user_message: "work",
            messages_json: "[3]",
            iterations: 1,
            usage_json: "{}",
            run_id: Some("run"),
            round_id: Some("first"),
            round_no: Some(1)
        })
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
