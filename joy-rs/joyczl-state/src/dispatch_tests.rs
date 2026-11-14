use crate::{dispatch::TaskDispatch, NewTask, TaskStatus, Tasks};

async fn fixture() -> (tempfile::TempDir, sqlx::SqlitePool, Tasks, TaskDispatch) {
    let dir = tempfile::tempdir().unwrap();
    let pool = crate::open(&dir.path().join("state.db")).await.unwrap();
    (
        dir,
        pool.clone(),
        Tasks::new(pool.clone()),
        TaskDispatch::new(pool),
    )
}
async fn add(tasks: &Tasks, id: &str) {
    tasks
        .create(NewTask {
            id,
            session_id: Some("chat"),
            title: id,
            goal_condition: None,
            acceptance_json: "[]",
            budget_json: "{}",
            cwd: None,
            max_rounds: 1,
            tier: None,
        })
        .await
        .unwrap();
}
#[tokio::test]
async fn dispatch_claim_is_single_and_keeps_conversation_separate() {
    let (_dir, _pool, tasks, queue) = fixture().await;
    add(&tasks, "one").await;
    add(&tasks, "two").await;
    queue.enqueue("one", "first").await.unwrap();
    queue.enqueue("two", "second").await.unwrap();
    assert!(tasks.active("chat").await.unwrap().is_none());
    assert_eq!(tasks.latest("chat").await.unwrap().unwrap().id, "two");
    let (a, b) = tokio::join!(queue.claim(), queue.claim());
    let claimed = [a.unwrap(), b.unwrap()];
    assert_eq!(claimed.iter().flatten().count(), 1);
    let first = claimed.into_iter().flatten().next().unwrap();
    assert_eq!(first.task_id, "one");
    assert_ne!(first.session_id, "chat");
    assert!(queue.claim().await.unwrap().is_none());
    queue.finish("one").await.unwrap();
    assert_eq!(queue.claim().await.unwrap().unwrap().task_id, "two");
    assert!(queue.enqueue("one", "do not repeat").await.is_err());
}
#[tokio::test]
async fn interrupted_claim_without_checkpoint_is_not_replayed() {
    let (_dir, _pool, tasks, queue) = fixture().await;
    add(&tasks, "one").await;
    queue.enqueue("one", "first").await.unwrap();
    queue.claim().await.unwrap().unwrap();
    assert_eq!(queue.reconcile().await.unwrap(), vec!["one"]);
    assert_eq!(
        tasks.get("one").await.unwrap().unwrap().status,
        TaskStatus::NeedsAttention
    );
    assert!(queue.claim().await.unwrap().is_none());
    assert!(queue.reconcile().await.unwrap().is_empty());
    assert!(queue.resume_unstarted("one").await.unwrap());
    assert_eq!(
        tasks.get("one").await.unwrap().unwrap().status,
        TaskStatus::Pending
    );
    assert_eq!(queue.claim().await.unwrap().unwrap().task_id, "one");
}
#[tokio::test]
async fn existing_checkpoint_owns_recovery_and_pending_dispatch_survives_restart() {
    let (_dir, pool, tasks, queue) = fixture().await;
    add(&tasks, "one").await;
    add(&tasks, "two").await;
    let row = queue.enqueue("one", "first").await.unwrap();
    queue.enqueue("two", "second").await.unwrap();
    queue.claim().await.unwrap().unwrap();
    crate::Checkpoints::new(pool.clone())
        .save(&row.turn_id, &row.session_id, "first", "[]", 0, "{}", None)
        .await
        .unwrap();
    assert!(queue.reconcile().await.unwrap().is_empty());
    assert_eq!(
        tasks.get("one").await.unwrap().unwrap().status,
        TaskStatus::Pending
    );
    assert_eq!(queue.claim().await.unwrap().unwrap().task_id, "two");
}
#[tokio::test]
async fn paused_and_cancelled_dispatches_do_not_run() {
    let (_dir, pool, tasks, queue) = fixture().await;
    for id in ["paused", "cancelled"] {
        add(&tasks, id).await;
        queue.enqueue(id, id).await.unwrap();
    }
    sqlx::query("UPDATE agent_tasks SET status='suspended' WHERE id='paused'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE agent_tasks SET status='cancelled' WHERE id='cancelled'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(queue.claim().await.unwrap().is_none());
    sqlx::query("UPDATE agent_tasks SET status='pending' WHERE id='paused'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(queue.claim().await.unwrap().unwrap().task_id, "paused");
}

#[tokio::test]
async fn progress_is_owner_scoped_and_does_not_claim_unverified_completion() {
    let (_dir, pool, tasks, queue) = fixture().await;
    add(&tasks, "one").await;
    queue.enqueue("one", "work").await.unwrap();
    sqlx::query("UPDATE agent_tasks SET status='done' WHERE id='one'")
        .execute(&pool)
        .await
        .unwrap();
    let rows = queue.progress("chat").await.unwrap();
    assert_eq!(rows[0]["status"], "produced_unverified");
    assert!(queue.progress("another-chat").await.unwrap().is_empty());
}
