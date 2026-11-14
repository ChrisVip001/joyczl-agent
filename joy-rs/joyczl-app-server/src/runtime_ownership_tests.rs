use joyczl_state::{NewTask, TaskStatus};

fn settings(home: &std::path::Path) -> joyczl_config::Settings {
    joyczl_config::Settings {
        home: home.into(),
        provider: "test-unconfigured".into(),
        ..Default::default()
    }
}
#[tokio::test]
async fn another_open_runtime_cannot_reconcile_a_live_owners_state() {
    let home = tempfile::tempdir().unwrap();
    let configuration = settings(home.path());
    let first = crate::open(&configuration).await.unwrap();
    first
        .tasks
        .create(NewTask {
            id: "task",
            session_id: Some("chat"),
            title: "live work",
            goal_condition: None,
            acceptance_json: "[]",
            budget_json: "{}",
            cwd: None,
            max_rounds: 3,
            tier: None,
        })
        .await
        .unwrap();
    first
        .runs
        .start("run", "task", Some("work"), Some("turn"))
        .await
        .unwrap();
    first.tasks.set_running("task", "turn").await.unwrap();
    first
        .checkpoints
        .save_round(
            "turn",
            "work",
            "work",
            "[]",
            0,
            "{}",
            Some("run"),
            Some("round"),
        )
        .await
        .unwrap();
    let _actor = first.begin_turn("turn", "work").unwrap();
    assert!(crate::open(&configuration).await.is_err());
    assert_eq!(
        first.tasks.get("task").await.unwrap().unwrap().status,
        TaskStatus::Running
    );
    assert_eq!(
        first.runs.get("run").await.unwrap().unwrap().status,
        "running"
    );
    assert_eq!(
        first.checkpoints.get("turn").await.unwrap().unwrap().status,
        "open"
    );
    first.finish_turn("turn");
}
#[tokio::test]
async fn the_last_runtime_clone_releases_execution_ownership() {
    let home = tempfile::tempdir().unwrap();
    let configuration = settings(home.path());
    let first = crate::open(&configuration).await.unwrap();
    let owner = first.clone();
    drop(first);
    assert!(crate::open(&configuration).await.is_err());
    drop(owner);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match crate::open(&configuration).await {
                Ok(next) => break next,
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .unwrap();
}
#[cfg(unix)]
#[tokio::test]
async fn database_symlinks_share_the_same_execution_owner() {
    let root = tempfile::tempdir().unwrap();
    let first_home = root.path().join("first");
    let second_home = root.path().join("second");
    std::fs::create_dir_all(&second_home).unwrap();
    let _first = crate::open(&settings(&first_home)).await.unwrap();
    std::os::unix::fs::symlink(first_home.join("state.db"), second_home.join("state.db")).unwrap();
    assert!(crate::open(&settings(&second_home)).await.is_err());
}

#[tokio::test]
async fn a_background_process_keeps_execution_ownership_after_the_client_drops() {
    if !joyczl_tools::exec::sandbox_available() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut configuration = settings(home.path());
    configuration.exec_enabled = true;
    configuration.exec_allow = vec!["sleep*".into()];
    let server = crate::open(&configuration).await.unwrap();
    let pool = server.pool.clone();
    let jobs = server.jobs.as_ref().unwrap().clone();
    let job = jobs
        .spawn(
            "owner",
            "sleep 0.2",
            Some(workspace.path().display().to_string()),
        )
        .await
        .unwrap();
    drop(server);
    assert!(crate::open(&configuration).await.is_err());
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let row = joyczl_state::Jobs::new(pool.clone())
                .get(&job)
                .await
                .unwrap()
                .unwrap();
            if row.status != "running" {
                assert_eq!(row.status, "completed");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    drop(jobs);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Ok(next) = crate::open(&configuration).await {
                break next;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
