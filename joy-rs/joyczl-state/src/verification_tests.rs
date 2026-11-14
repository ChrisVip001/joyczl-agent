use crate::{
    verification::{FileMeasurement, Verifications},
    NewTask, Tasks,
};

#[tokio::test]
async fn receipts_bind_versions_roll_back_with_checks_and_follow_reverted_content() {
    let home = tempfile::tempdir().unwrap();
    let pool = crate::open(&home.path().join("state.db")).await.unwrap();
    let tasks = Tasks::new(pool.clone());
    let acceptance = r#"[{"kind":"fileExists","path":"report.md"}]"#;
    tasks
        .create(NewTask {
            id: "task",
            session_id: None,
            title: "report",
            goal_condition: None,
            acceptance_json: acceptance,
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
    tasks.set_running("task", "turn").await.unwrap();
    let file = home.path().join("report.md");
    std::fs::write(&file, "Version A").unwrap();
    let original = FileMeasurement::read(&file).unwrap();
    let store = Verifications::new(pool.clone());
    let checks = r#"[{"label":"report exists","passed":true,"detail":"Measured file"}]"#;
    assert!(tasks
        .finish_run(
            "task",
            crate::TaskStatus::Done,
            Some("No evidence yet"),
            Some("run")
        )
        .await
        .is_err());
    assert_eq!(
        tasks.get("task").await.unwrap().unwrap().status,
        crate::TaskStatus::Running
    );
    let first = store
        .record(
            "task",
            "run",
            "round",
            acceptance,
            checks,
            std::slice::from_ref(&original),
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .record(
                "task",
                "run",
                "round",
                acceptance,
                checks,
                std::slice::from_ref(&original)
            )
            .await
            .unwrap(),
        first
    );
    std::fs::write(&file, "Version B").unwrap();
    let changed = FileMeasurement::read(&file).unwrap();
    assert_ne!(original.sha256, changed.sha256);
    sqlx::query("CREATE TRIGGER fail_verification_receipt BEFORE INSERT ON task_verifications BEGIN SELECT RAISE(FAIL,'injected verification receipt failure'); END").execute(&pool).await.unwrap();
    let different_checks =
        r#"[{"label":"report exists","passed":true,"detail":"New measured version"}]"#;
    assert!(store
        .record(
            "task",
            "run",
            "round",
            acceptance,
            different_checks,
            std::slice::from_ref(&changed)
        )
        .await
        .is_err());
    assert_eq!(
        tasks.get("task").await.unwrap().unwrap().checks_json,
        checks
    );
    assert_eq!(store.latest("task").await.unwrap().unwrap().id, first);
    sqlx::query("DROP TRIGGER fail_verification_receipt")
        .execute(&pool)
        .await
        .unwrap();
    let second = store
        .record("task", "run", "round", acceptance, checks, &[changed])
        .await
        .unwrap();
    let reverted = store
        .record("task", "run", "round", acceptance, checks, &[original])
        .await
        .unwrap();
    assert_ne!(reverted, first);
    assert_eq!(store.latest("task").await.unwrap().unwrap().id, reverted);
    assert_eq!(
        store.get(&reverted).await.unwrap().unwrap().previous_id,
        Some(second)
    );
    let mut tampered = store.get(&reverted).await.unwrap().unwrap();
    tampered.files_json = "[]".into();
    assert!(tampered.verify().is_err());
    let mut tampered_time = store.get(&reverted).await.unwrap().unwrap();
    tampered_time.measured_at = "2000-01-01T00:00:00Z".into();
    assert!(tampered_time.verify().is_err());
    tasks
        .finish_run(
            "task",
            crate::TaskStatus::Done,
            Some("Verified"),
            Some("run"),
        )
        .await
        .unwrap();
    let attached: Option<String> = sqlx::query_scalar(
        "SELECT verification_id FROM task_completions WHERE task_id='task' AND status='done'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(attached, Some(reverted));
}

#[test]
fn text_and_streaming_measurements_fingerprint_the_same_bytes() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("report.md");
    std::fs::write(&path, "Approved figures: 42\n").unwrap();
    let (text, measured) = FileMeasurement::read_text(&path).unwrap();
    assert_eq!(text, "Approved figures: 42\n");
    assert_eq!(measured, FileMeasurement::read(&path).unwrap());
    assert_eq!(measured.sha256, crate::ledger::digest_of(text.as_bytes()));
    assert!(FileMeasurement::read(home.path()).is_err());
}
