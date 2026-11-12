use crate::plan::{self, Plan};
use joyczl_state::Steps;

#[tokio::test]
async fn a_failed_plan_commit_leaves_no_partial_plan() {
    let home = tempfile::tempdir().unwrap();
    let pool = joyczl_state::open(&home.path().join("state.db"))
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER fail_plan BEFORE INSERT ON task_steps WHEN NEW.kind='plan' AND NEW.seq=2 BEGIN SELECT RAISE(FAIL, 'injected'); END").execute(&pool).await.unwrap();
    let steps = Steps::new(pool);
    let saved = plan::record(
        &steps,
        "task",
        &Plan {
            version: 1,
            steps: vec!["one".into(), "two".into()],
        },
    )
    .await;
    assert!(saved.is_err());
    assert!(plan::restore(&steps, "task").await.unwrap().is_none());
    assert!(
        steps.list_plan("task").await.unwrap().is_empty(),
        "a partial plan cannot be recovered safely"
    );
}

#[tokio::test]
async fn complete_versions_survive_reopen_and_cannot_be_rewritten() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("state.db");
    let pool = joyczl_state::open(&path).await.unwrap();
    let steps = Steps::new(pool.clone());
    let first = Plan {
        version: 1,
        steps: vec!["one".into(), "two".into()],
    };
    plan::record(&steps, "task", &first).await.unwrap();
    plan::record(&steps, "task", &first).await.unwrap();
    let revised = Plan {
        version: 2,
        steps: vec!["revised".into()],
    };
    plan::record(&steps, "task", &revised).await.unwrap();
    assert!(plan::record(
        &steps,
        "task",
        &Plan {
            version: 2,
            steps: vec!["different".into()]
        }
    )
    .await
    .is_err());
    pool.close().await;
    let reopened = Steps::new(joyczl_state::open(&path).await.unwrap());
    assert_eq!(
        plan::restore(&reopened, "task").await.unwrap(),
        Some(revised)
    );
    assert!(plan::restore(&reopened, "other").await.unwrap().is_none());
    assert_eq!(reopened.list_plan("task").await.unwrap().len(), 3);
}
