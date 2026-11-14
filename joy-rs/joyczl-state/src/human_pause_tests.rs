use crate::{human_pauses::HumanPauses, Checkpoints, NewTask, TaskStatus, Tasks};

#[tokio::test]
async fn checkpoint_writes_cannot_clear_human_pause_authority() {
    let home = tempfile::tempdir().unwrap();
    let pool = crate::open(&home.path().join("state.db")).await.unwrap();
    let tasks = Tasks::new(pool.clone());
    tasks
        .create(NewTask {
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
    tasks.set_running("task", "turn").await.unwrap();
    tasks.suspend("task", Some("Human pause")).await.unwrap();
    Checkpoints::new(pool.clone())
        .save("turn", "chat", "work", "[]", 0, "{}", None)
        .await
        .unwrap();
    let pauses = HumanPauses::new(pool.clone());
    assert!(pauses.is_paused("task").await.unwrap());
    assert!(tasks.set_running("task", "turn").await.is_err());
    assert!(tasks.active("chat").await.unwrap().is_none());
    assert_eq!(
        tasks.latest("chat").await.unwrap().unwrap().status,
        TaskStatus::Suspended
    );
    let reopened = crate::open(&home.path().join("state.db")).await.unwrap();
    assert!(HumanPauses::new(reopened).is_paused("task").await.unwrap());
    pauses.authorize_resume("task").await.unwrap();
    tasks.set_running("task", "turn").await.unwrap();
    assert_eq!(
        tasks.active("chat").await.unwrap().unwrap().status,
        TaskStatus::Running
    );
}

#[tokio::test]
async fn a_failed_pause_transition_does_not_leave_a_hidden_fence() {
    let home = tempfile::tempdir().unwrap();
    let pool = crate::open(&home.path().join("state.db")).await.unwrap();
    let tasks = Tasks::new(pool.clone());
    tasks
        .create(NewTask {
            id: "task",
            session_id: None,
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
    tasks.set_running("task", "turn").await.unwrap();
    sqlx::query("CREATE TRIGGER fail_pause BEFORE INSERT ON task_completions BEGIN SELECT RAISE(FAIL,'injected pause failure'); END").execute(&pool).await.unwrap();
    assert!(tasks.suspend("task", Some("Human pause")).await.is_err());
    assert!(!HumanPauses::new(pool).is_paused("task").await.unwrap());
    assert_eq!(
        tasks.get("task").await.unwrap().unwrap().status,
        TaskStatus::Running
    );
}

#[tokio::test]
async fn continuation_failure_keeps_the_pause_and_original_boundary() {
    let home = tempfile::tempdir().unwrap();
    let pool = crate::open(&home.path().join("state.db")).await.unwrap();
    let tasks = Tasks::new(pool.clone());
    tasks
        .create(NewTask {
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
    crate::Runs::new(pool.clone())
        .start("run", "task", Some("chat"), Some("turn"))
        .await
        .unwrap();
    let checkpoints = Checkpoints::new(pool.clone());
    checkpoints
        .save("turn", "chat", "work", "[]", 0, "{}", Some("run"))
        .await
        .unwrap();
    checkpoints.finish("turn", "suspended").await.unwrap();
    tasks.suspend("task", Some("Human pause")).await.unwrap();
    let pauses = HumanPauses::new(pool.clone());
    sqlx::query("CREATE TRIGGER fail_continuation BEFORE UPDATE OF messages_json ON turn_checkpoints BEGIN SELECT RAISE(FAIL,'injected continuation failure'); END").execute(&pool).await.unwrap();
    assert!(pauses
        .resume_checkpoint("task", "turn", "[1]")
        .await
        .is_err());
    assert!(pauses.is_paused("task").await.unwrap());
    assert_eq!(
        tasks.get("task").await.unwrap().unwrap().status,
        TaskStatus::Suspended
    );
    assert_eq!(
        checkpoints
            .get("turn")
            .await
            .unwrap()
            .unwrap()
            .messages_json,
        "[]"
    );
    sqlx::query("DROP TRIGGER fail_continuation")
        .execute(&pool)
        .await
        .unwrap();
    pauses
        .resume_checkpoint("task", "turn", "[1]")
        .await
        .unwrap();
    assert!(!pauses.is_paused("task").await.unwrap());
    assert_eq!(
        tasks.get("task").await.unwrap().unwrap().status,
        TaskStatus::Running
    );
    let checkpoint = checkpoints.get("turn").await.unwrap().unwrap();
    assert_eq!(checkpoint.status, "open");
    assert_eq!(checkpoint.messages_json, "[1]");
}
