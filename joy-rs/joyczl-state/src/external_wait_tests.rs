use crate::{external_waits::*, Checkpoints, NewTask, Runs, TaskStatus, Tasks};

async fn fixture() -> (
    tempfile::TempDir,
    sqlx::SqlitePool,
    ExternalWaits,
    InputWait,
) {
    let home = tempfile::tempdir().unwrap();
    let pool = crate::open(&home.path().join("state.db")).await.unwrap();
    let tasks = Tasks::new(pool.clone());
    tasks
        .create(NewTask {
            id: "task",
            session_id: Some("owner"),
            title: "report",
            goal_condition: None,
            acceptance_json: "[]",
            budget_json: "{}",
            cwd: None,
            max_rounds: 3,
            tier: None,
        })
        .await
        .unwrap();
    let inputs = ExternalWaits::new(pool.clone());
    inputs
        .define(NewInputCondition {
            condition_id: "input",
            task_id: "task",
            source: "authorized-host",
            event_key: "meeting-figures",
            description: "Wait for the approved figures",
            expires_at: None,
        })
        .await
        .unwrap();
    Runs::new(pool.clone())
        .start("run", "task", Some("work"), Some("turn"))
        .await
        .unwrap();
    tasks.set_running("task", "turn").await.unwrap();
    Checkpoints::new(pool.clone())
        .save_numbered_round(
            "turn",
            "work",
            "prepare report",
            "[]",
            0,
            "{}",
            Some("run"),
            Some("round"),
            Some(1),
        )
        .await
        .unwrap();
    let wait = InputWait {
        task_id: "task".into(),
        condition_id: "input".into(),
        run_id: "run".into(),
        turn_id: "turn".into(),
        session_id: "work".into(),
        state: "waiting".into(),
    };
    (home, pool, inputs, wait)
}
fn snapshot() -> WaitSnapshot<'static> {
    WaitSnapshot {
        messages_json: r#"[{"draft":"keep the actual draft"}]"#,
        usage_json: r#"{"input_tokens":10,"output_tokens":5}"#,
        iterations: 2,
        round_id: "round",
        round_no: 1,
    }
}

#[tokio::test]
async fn durable_input_is_source_bound_immutable_and_claimed_once() {
    let (_home, pool, inputs, wait) = fixture().await;
    inputs.register(&wait, snapshot()).await.unwrap();
    assert!(inputs
        .signal(
            "input",
            "another-host",
            "meeting-figures",
            "event-1",
            r#"{"value":42}"#
        )
        .await
        .is_err());
    assert!(inputs
        .signal(
            "input",
            "authorized-host",
            "another-key",
            "event-1",
            r#"{"value":42}"#
        )
        .await
        .is_err());
    assert!(inputs
        .signal(
            "input",
            "authorized-host",
            "meeting-figures",
            "event-1",
            r#"{"value":42}"#
        )
        .await
        .unwrap());
    assert!(!inputs
        .signal(
            "input",
            "authorized-host",
            "meeting-figures",
            "event-1",
            r#"{"value":42}"#
        )
        .await
        .unwrap());
    assert!(inputs
        .signal(
            "input",
            "authorized-host",
            "meeting-figures",
            "event-1",
            r#"{"value":99}"#
        )
        .await
        .is_err());
    assert!(inputs
        .signal(
            "input",
            "authorized-host",
            "meeting-figures",
            "event-2",
            r#"{"value":99}"#
        )
        .await
        .is_err());
    let cp = Checkpoints::new(pool.clone())
        .get("turn")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cp.messages_json, snapshot().messages_json);
    assert_eq!(cp.status, "waiting_input");
    let reopened = ExternalWaits::new(pool.clone());
    let (first, second) = tokio::join!(
        inputs.claim(&wait, "[\"data received\"]"),
        reopened.claim(&wait, "[\"data received\"]")
    );
    assert_eq!(
        usize::from(first.unwrap()) + usize::from(second.unwrap()),
        1
    );
    let cp = Checkpoints::new(pool.clone())
        .get("turn")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cp.status, "open");
    assert_eq!(cp.round_no, Some(1));
    assert_eq!(cp.usage_json, snapshot().usage_json);
    assert_eq!(
        reopened
            .get("task")
            .await
            .unwrap()
            .unwrap()
            .consumed_run_id
            .as_deref(),
        Some("run")
    );
}

#[tokio::test]
async fn human_pause_and_failed_wake_keep_the_input_pending() {
    let (_home, pool, inputs, wait) = fixture().await;
    inputs.register(&wait, snapshot()).await.unwrap();
    inputs
        .signal(
            "input",
            "authorized-host",
            "meeting-figures",
            "event-1",
            "42",
        )
        .await
        .unwrap();
    let tasks = Tasks::new(pool.clone());
    tasks.suspend("task", Some("Human pause")).await.unwrap();
    assert!(!inputs.claim(&wait, "[]").await.unwrap());
    assert_eq!(
        tasks
            .get("task")
            .await
            .unwrap()
            .unwrap()
            .suspend_reason
            .as_deref(),
        Some("Human pause")
    );
    tasks
        .suspend("task", Some(INPUT_WAIT_REASON))
        .await
        .unwrap();
    assert!(!inputs.claim(&wait, "[]").await.unwrap());
    assert_eq!(
        inputs.waiting("task").await.unwrap().unwrap().state,
        "cancelled"
    );
    crate::human_pauses::HumanPauses::new(pool.clone())
        .resume_checkpoint("task", &wait.turn_id, "[]")
        .await
        .unwrap();
    crate::Runs::new(pool.clone())
        .reopen(&wait.run_id)
        .await
        .unwrap();
    inputs.register(&wait, snapshot()).await.unwrap();
    sqlx::query("CREATE TRIGGER fail_wake BEFORE UPDATE OF messages_json ON turn_checkpoints BEGIN SELECT RAISE(FAIL,'injected wake failure'); END").execute(&pool).await.unwrap();
    assert!(inputs.claim(&wait, "[1]").await.is_err());
    assert_eq!(
        inputs.waiting("task").await.unwrap().unwrap().state,
        "waiting"
    );
    assert_eq!(
        tasks.get("task").await.unwrap().unwrap().status,
        TaskStatus::Suspended
    );
    sqlx::query("DROP TRIGGER fail_wake")
        .execute(&pool)
        .await
        .unwrap();
    assert!(inputs.claim(&wait, "[1]").await.unwrap());
}

#[tokio::test]
async fn only_prompted_input_satisfies_the_current_run() {
    let (_home, _pool, inputs, _wait) = fixture().await;
    let before = inputs.for_prompt("task", "run").await.unwrap().unwrap();
    assert!(before.consumed_run_id.is_none());
    inputs
        .signal(
            "input",
            "authorized-host",
            "meeting-figures",
            "event-1",
            "42",
        )
        .await
        .unwrap();
    assert!(inputs
        .get("task")
        .await
        .unwrap()
        .unwrap()
        .consumed_run_id
        .is_none());
    let prompted = inputs.for_prompt("task", "run").await.unwrap().unwrap();
    assert_eq!(prompted.consumed_run_id.as_deref(), Some("run"));
    assert_eq!(prompted.payload_json.as_deref(), Some("42"));
    assert!(inputs
        .define(NewInputCondition {
            condition_id: "replacement",
            task_id: "task",
            source: "other",
            event_key: "key",
            description: "new scope",
            expires_at: None
        })
        .await
        .is_err());
}

#[tokio::test]
async fn input_wait_admission_rolls_back_and_deadlines_only_limit_arrival() {
    let (_home, pool, inputs, wait) = fixture().await;
    assert!(inputs.for_prompt("task", "foreign-run").await.is_err());
    sqlx::query("CREATE TRIGGER fail_input_wait BEFORE INSERT ON task_input_waits BEGIN SELECT RAISE(FAIL,'injected input wait failure'); END").execute(&pool).await.unwrap();
    assert!(inputs.register(&wait, snapshot()).await.is_err());
    assert_eq!(
        Tasks::new(pool.clone())
            .get("task")
            .await
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Running
    );
    assert_eq!(
        Runs::new(pool.clone())
            .get("run")
            .await
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
    assert_eq!(
        Checkpoints::new(pool.clone())
            .get("turn")
            .await
            .unwrap()
            .unwrap()
            .status,
        "open"
    );
    assert!(inputs.waiting("task").await.unwrap().is_none());
    let receipts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM task_completions WHERE task_id='task'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(receipts, 0);
    sqlx::query("DROP TRIGGER fail_input_wait")
        .execute(&pool)
        .await
        .unwrap();
    inputs.register(&wait, snapshot()).await.unwrap();
    sqlx::query("UPDATE task_input_conditions SET expires_at='2000-01-01T00:00:00Z'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(inputs.expired("task").await.unwrap());
    assert!(inputs
        .signal(
            "input",
            "authorized-host",
            "meeting-figures",
            "late-event",
            "42"
        )
        .await
        .is_err());
    sqlx::query("UPDATE task_input_conditions SET expires_at=NULL")
        .execute(&pool)
        .await
        .unwrap();
    inputs
        .signal(
            "input",
            "authorized-host",
            "meeting-figures",
            "on-time-event",
            "42",
        )
        .await
        .unwrap();
    sqlx::query("UPDATE task_input_conditions SET expires_at='2000-01-01T00:00:00Z'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(!inputs.expired("task").await.unwrap());
    assert!(inputs.claim(&wait, "[]").await.unwrap());
}
