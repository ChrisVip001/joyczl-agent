use crate::{
    schedule_completions::{ScheduleCompletion, ScheduleCompletions},
    Schedules,
};

#[tokio::test]
async fn schedule_settlement_and_production_rollback_together() {
    let home = tempfile::tempdir().unwrap();
    let pool = crate::open(&home.path().join("state.db")).await.unwrap();
    let schedules = Schedules::new(pool.clone());
    let rows = ScheduleCompletions::new(pool.clone());
    schedules.ensure("job", true, "notify").await.unwrap();
    schedules.claim("job", "2026-11-13T08:00").await.unwrap();
    sqlx::query("CREATE TRIGGER fail_schedule_notice BEFORE INSERT ON schedule_completions BEGIN SELECT RAISE(FAIL,'injected schedule completion failure'); END").execute(&pool).await.unwrap();
    let completion = || ScheduleCompletion {
        name: "job",
        minute: "2026-11-13T08:00",
        status: "produced",
        detail: "",
        output: "body",
        preview: "preview",
        output_path: Some("report.md"),
        notify_requested: true,
        notify_target: Some("original"),
    };
    assert!(rows.settle(completion()).await.is_err());
    assert_eq!(
        schedules.pending_claim("job").await.unwrap().as_deref(),
        Some("2026-11-13T08:00")
    );
    assert!(schedules.last_fired("job").await.unwrap().is_none());
    assert!(rows.pending_outputs().await.unwrap().is_empty());
    sqlx::query("DROP TRIGGER fail_schedule_notice")
        .execute(&pool)
        .await
        .unwrap();
    rows.settle(completion()).await.unwrap();
    assert!(schedules.pending_claim("job").await.unwrap().is_none());
    assert_eq!(
        schedules.last_fired("job").await.unwrap().as_deref(),
        Some("2026-11-13T08:00")
    );
    rows.settle(ScheduleCompletion {
        output: "cannot replace the original",
        ..completion()
    })
    .await
    .unwrap();
    assert_eq!(rows.pending_outputs().await.unwrap()[0].output, "body");
    assert!(rows.pending().await.unwrap().is_empty());
    rows.output_written("job", "2026-11-13T08:00")
        .await
        .unwrap();
    let notices = rows.pending().await.unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].notify_target.as_deref(), Some("original"));
    schedules.claim("job", "2026-11-13T08:01").await.unwrap();
    rows.settle(ScheduleCompletion {
        name: "job",
        minute: "2026-11-13T08:01",
        status: "empty",
        detail: "",
        output: "",
        preview: "",
        output_path: None,
        notify_requested: false,
        notify_target: None,
    })
    .await
    .unwrap();
    assert!(
        !schedules
            .claim_once("job", "2026-11-13T08:00")
            .await
            .unwrap(),
        "An older settled trigger cannot be replayed after the clock moves backwards"
    );
}

#[tokio::test]
async fn an_uncertain_schedule_stays_claimed_until_human_review() {
    let home = tempfile::tempdir().unwrap();
    let pool = crate::open(&home.path().join("state.db")).await.unwrap();
    let schedules = Schedules::new(pool.clone());
    let rows = ScheduleCompletions::new(pool.clone());
    schedules.ensure("job", true, "outbox").await.unwrap();
    schedules.claim("job", "2026-11-13T08:00").await.unwrap();
    rows.settle(ScheduleCompletion {
        name: "job",
        minute: "2026-11-13T08:00",
        status: "needs_attention",
        detail: "unknown effect",
        output: "",
        preview: "unknown",
        output_path: None,
        notify_requested: false,
        notify_target: None,
    })
    .await
    .unwrap();
    assert!(!schedules
        .claim_once("job", "2026-11-13T08:01")
        .await
        .unwrap());
    assert!(schedules.last_fired("job").await.unwrap().is_none());
    assert_eq!(
        rows.acknowledge("job").await.unwrap().as_deref(),
        Some("2026-11-13T08:00")
    );
    assert!(rows.acknowledge("job").await.unwrap().is_none());
    assert!(!schedules
        .claim_once("job", "2026-11-13T08:00")
        .await
        .unwrap());
    assert!(schedules
        .claim_once("job", "2026-11-13T08:01")
        .await
        .unwrap());
    let reviewed: Option<String> =
        sqlx::query_scalar("SELECT reviewed_at FROM schedule_completions WHERE name='job'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(reviewed.is_some());
}

#[tokio::test]
async fn competing_trigger_claims_admit_only_one_turn() {
    let home = tempfile::tempdir().unwrap();
    let pool = crate::open(&home.path().join("state.db")).await.unwrap();
    let schedules = Schedules::new(pool);
    schedules.ensure("job", true, "quiet").await.unwrap();
    let (a, b) = tokio::join!(
        schedules.claim_once("job", "2026-11-13T08:00"),
        schedules.claim_once("job", "2026-11-13T08:01")
    );
    assert_eq!(usize::from(a.unwrap()) + usize::from(b.unwrap()), 1);
}
