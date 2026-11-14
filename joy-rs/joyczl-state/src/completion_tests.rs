use crate::{completions::Completions, NewTask, TaskStatus, Tasks};

#[tokio::test]
async fn a_failed_notice_insert_rolls_back_the_task_transition() {
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
    let completions = Completions::new(pool.clone());
    completions
        .bind("task", true, Some("original"))
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER fail_notice BEFORE INSERT ON task_completions BEGIN SELECT RAISE(FAIL,'injected completion failure'); END").execute(&pool).await.unwrap();
    assert!(tasks.finish("task", TaskStatus::Done).await.is_err());
    assert_eq!(
        tasks.get("task").await.unwrap().unwrap().status,
        TaskStatus::Pending
    );
    assert!(completions.pending().await.unwrap().is_empty());
    sqlx::query("DROP TRIGGER fail_notice")
        .execute(&pool)
        .await
        .unwrap();
    tasks.finish("task", TaskStatus::Done).await.unwrap();
    tasks.finish("task", TaskStatus::Done).await.unwrap();
    let rows = completions.pending().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "produced_unverified");
    assert_eq!(rows[0].notify_target.as_deref(), Some("original"));
}

#[tokio::test]
async fn task_and_exact_run_completion_commit_or_rollback_together() {
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
    let runs = crate::Runs::new(pool.clone());
    runs.start("run", "task", Some("chat"), Some("turn"))
        .await
        .unwrap();
    tasks.set_running("task", "turn").await.unwrap();
    sqlx::query("CREATE TRIGGER fail_completion BEFORE INSERT ON task_completions BEGIN SELECT RAISE(FAIL,'injected completion failure'); END").execute(&pool).await.unwrap();
    assert!(tasks
        .finish_run("task", TaskStatus::Done, None, Some("run"))
        .await
        .is_err());
    assert_eq!(
        tasks.get("task").await.unwrap().unwrap().status,
        TaskStatus::Running
    );
    assert_eq!(runs.get("run").await.unwrap().unwrap().status, "running");
    sqlx::query("DROP TRIGGER fail_completion")
        .execute(&pool)
        .await
        .unwrap();
    tasks
        .finish_run("task", TaskStatus::Done, None, Some("run"))
        .await
        .unwrap();
    assert_eq!(
        tasks.get("task").await.unwrap().unwrap().status,
        TaskStatus::Done
    );
    assert_eq!(runs.get("run").await.unwrap().unwrap().status, "completed");
    let receipt_run: Option<String> = sqlx::query_scalar("SELECT run_id FROM task_completions WHERE task_id='task' AND status='produced_unverified' ORDER BY id DESC LIMIT 1").fetch_one(&pool).await.unwrap();
    assert_eq!(receipt_run.as_deref(), Some("run"));
}

#[tokio::test]
async fn a_model_judged_goal_without_acceptance_is_produced_unverified() {
    let home = tempfile::tempdir().unwrap();
    let pool = crate::open(&home.path().join("state.db")).await.unwrap();
    let tasks = Tasks::new(pool.clone());
    tasks
        .create(NewTask {
            id: "goal",
            session_id: Some("chat"),
            title: "research",
            goal_condition: Some("Prepare a useful research summary"),
            acceptance_json: "[]",
            budget_json: "{}",
            cwd: None,
            max_rounds: 3,
            tier: None,
        })
        .await
        .unwrap();
    Completions::new(pool.clone())
        .bind("goal", true, Some("test-target"))
        .await
        .unwrap();
    tasks.finish("goal", TaskStatus::Done).await.unwrap();
    let receipt = Completions::new(pool).pending().await.unwrap().remove(0);
    assert!(receipt.is_goal);
    assert_eq!(receipt.status, "produced_unverified");
}
