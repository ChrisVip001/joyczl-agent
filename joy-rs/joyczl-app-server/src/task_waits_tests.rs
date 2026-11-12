use super::*;
use joyczl_provider::{mock::Mock, Resolved};
use std::sync::Arc;

async fn fixture() -> (Server, Arc<Mock>, tempfile::TempDir) {
    let home = tempfile::tempdir().unwrap();
    let pool = joyczl_state::open(&home.path().join("state.db"))
        .await
        .unwrap();
    let server = Server::boot(
        pool,
        joyczl_config::Settings {
            home: home.path().into(),
            ..Default::default()
        },
    )
    .await;
    let mock = Arc::new(Mock::new(vec![
        Mock::text(r#"{"retrieve":false,"query":"","reason":"task"}"#),
        Mock::text("Command started."),
        Mock::text("Verified the completed result."),
    ]));
    server.set_resolved(Some(Resolved {
        context_window: joyczl_provider::DEFAULT_CONTEXT_WINDOW,
        vision: false,
        provider_id: "mock".into(),
        client: mock.clone(),
        model: "test".into(),
        small_model: "test".into(),
    }));
    server
        .tasks
        .create(joyczl_state::NewTask {
            id: "waiting-task",
            title: "work",
            session_id: Some("s"),
            goal_condition: None,
            cwd: None,
            acceptance_json: "[]",
            budget_json: "{}",
            max_rounds: 2,
            tier: None,
        })
        .await
        .unwrap();
    server
        .runs
        .start("prior-run", "waiting-task", Some("s"), None)
        .await
        .unwrap();
    joyczl_state::Jobs::new(server.pool.clone())
        .record_started_for_run("job", "s", "background", "output.log", Some("prior-run"))
        .await
        .unwrap();
    let (sink, _receiver) = EventSink::channel();
    crate::turn::run_turn(
        &server,
        joyczl_protocol::TurnStartParams {
            session_id: Some("s".into()),
            message: "continue the task".into(),
            ..Default::default()
        },
        joyczl_protocol::RequestId::Number(1),
        &sink,
    )
    .await
    .unwrap();
    let task = server.tasks.get("waiting-task").await.unwrap().unwrap();
    assert_eq!(task.status, joyczl_state::TaskStatus::Suspended);
    assert_eq!(task.suspend_reason.as_deref(), Some(JOB_WAIT_REASON));
    assert_eq!(mock.received.lock().unwrap().len(), 2);
    (server, mock, home)
}

async fn wait_for_status(server: &Server, status: joyczl_state::TaskStatus) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if server
                .tasks
                .get("waiting-task")
                .await
                .unwrap()
                .unwrap()
                .status
                == status
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("task did not reach expected status");
}

#[tokio::test]
async fn a_waiting_task_wakes_on_known_job_completion_and_verifies() {
    let (server, mock, _home) = fixture().await;
    joyczl_state::Jobs::new(server.pool.clone())
        .record_finished("job", "completed", Some(0))
        .await
        .unwrap();
    wait_for_status(&server, joyczl_state::TaskStatus::Done).await;
    {
        let requests = mock.received.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[2]
            .messages
            .iter()
            .any(|m| m.text().contains("job: completed")));
    }
    assert_eq!(
        JobWaits::new(server.pool.clone())
            .get("waiting-task")
            .await
            .unwrap()
            .unwrap()
            .state,
        "claimed"
    );
}

#[tokio::test]
async fn a_human_pause_prevents_job_completion_from_waking_the_task() {
    let (server, mock, _home) = fixture().await;
    let (sink, _receiver) = EventSink::channel();
    crate::task::suspend(
        &server,
        &sink,
        joyczl_protocol::TaskActionParams {
            task_id: "waiting-task".into(),
            reason: Some("pause now".into()),
        },
    )
    .await
    .unwrap();
    joyczl_state::Jobs::new(server.pool.clone())
        .record_finished("job", "completed", Some(0))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let task = server.tasks.get("waiting-task").await.unwrap().unwrap();
    assert_eq!(task.status, joyczl_state::TaskStatus::Suspended);
    assert_eq!(task.suspend_reason.as_deref(), Some("pause now"));
    assert_eq!(mock.received.lock().unwrap().len(), 2);
    assert_eq!(
        JobWaits::new(server.pool.clone())
            .get("waiting-task")
            .await
            .unwrap()
            .unwrap()
            .state,
        "cancelled"
    );
}

#[tokio::test]
async fn an_unknown_job_outcome_stops_for_review_without_reexecution() {
    let (server, mock, _home) = fixture().await;
    joyczl_state::Jobs::new(server.pool.clone())
        .record_finished("job", "interrupted", None)
        .await
        .unwrap();
    wait_for_status(&server, joyczl_state::TaskStatus::NeedsAttention).await;
    assert_eq!(mock.received.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn boot_restores_a_committed_wait_and_continues_after_the_job_finished() {
    let home = tempfile::tempdir().unwrap();
    let pool = joyczl_state::open(&home.path().join("state.db"))
        .await
        .unwrap();
    let tasks = joyczl_state::Tasks::new(pool.clone());
    tasks
        .create(joyczl_state::NewTask {
            id: "waiting-task",
            title: "recover",
            session_id: Some("s"),
            goal_condition: None,
            cwd: None,
            acceptance_json: "[]",
            budget_json: "{}",
            max_rounds: 2,
            tier: None,
        })
        .await
        .unwrap();
    tasks
        .set_running("waiting-task", "recover-run")
        .await
        .unwrap();
    joyczl_state::Runs::new(pool.clone())
        .start(
            "recover-run",
            "waiting-task",
            Some("s"),
            Some("recover-run"),
        )
        .await
        .unwrap();
    let jobs = joyczl_state::Jobs::new(pool.clone());
    jobs.record_started_for_run("job", "s", "background", "output.log", Some("recover-run"))
        .await
        .unwrap();
    let memory = serde_json::to_string(&vec![joyczl_provider::Message::user_text(
        "complete the task",
    )])
    .unwrap();
    joyczl_state::Checkpoints::new(pool.clone())
        .save_round(
            "recover-run",
            "s",
            "complete the task",
            &memory,
            0,
            r#"{"input_tokens":0,"output_tokens":0}"#,
            Some("recover-run"),
            Some("recovered-round"),
        )
        .await
        .unwrap();
    JobWaits::new(pool.clone())
        .register(
            "waiting-task",
            "recover-run",
            "recover-run",
            "s",
            &["job".into()],
        )
        .await
        .unwrap();
    jobs.record_finished("job", "completed", Some(0))
        .await
        .unwrap();
    let server = Server::boot(
        pool,
        joyczl_config::Settings {
            home: home.path().into(),
            ..Default::default()
        },
    )
    .await;
    let mock = Arc::new(Mock::new(vec![Mock::text("Verified after restart.")]));
    server.set_resolved(Some(Resolved {
        context_window: joyczl_provider::DEFAULT_CONTEXT_WINDOW,
        vision: false,
        provider_id: "mock".into(),
        client: mock.clone(),
        model: "test".into(),
        small_model: "test".into(),
    }));
    wait_for_status(&server, joyczl_state::TaskStatus::Done).await;
    assert_eq!(mock.received.lock().unwrap().len(), 1);
    assert_eq!(
        server
            .runs
            .get("recover-run")
            .await
            .unwrap()
            .unwrap()
            .rounds,
        1
    );
}

#[tokio::test]
async fn cancelling_a_waiting_task_prevents_later_completion_from_resuming_it() {
    let (server, mock, _home) = fixture().await;
    let (sink, _receiver) = EventSink::channel();
    crate::task::cancel(
        &server,
        &sink,
        joyczl_protocol::TaskActionParams {
            task_id: "waiting-task".into(),
            reason: None,
        },
    )
    .await
    .unwrap();
    joyczl_state::Jobs::new(server.pool.clone())
        .record_finished("job", "completed", Some(0))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert_eq!(
        server
            .tasks
            .get("waiting-task")
            .await
            .unwrap()
            .unwrap()
            .status,
        joyczl_state::TaskStatus::Cancelled
    );
    assert_eq!(mock.received.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn competing_wake_workers_continue_the_task_only_once() {
    let (server, mock, _home) = fixture().await;
    let wait = JobWaits::new(server.pool.clone())
        .get("waiting-task")
        .await
        .unwrap()
        .unwrap();
    let (sink, _receiver) = EventSink::channel();
    arm(server.clone(), sink.clone(), wait.clone());
    arm(server.clone(), sink, wait);
    joyczl_state::Jobs::new(server.pool.clone())
        .record_finished("job", "completed", Some(0))
        .await
        .unwrap();
    wait_for_status(&server, joyczl_state::TaskStatus::Done).await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert_eq!(mock.received.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn disabling_exec_after_restart_reconciles_running_jobs_instead_of_waiting_forever() {
    let home = tempfile::tempdir().unwrap();
    let pool = joyczl_state::open(&home.path().join("state.db"))
        .await
        .unwrap();
    let tasks = joyczl_state::Tasks::new(pool.clone());
    tasks
        .create(joyczl_state::NewTask {
            id: "waiting-task",
            title: "recover",
            session_id: Some("s"),
            goal_condition: None,
            cwd: None,
            acceptance_json: "[]",
            budget_json: "{}",
            max_rounds: 2,
            tier: None,
        })
        .await
        .unwrap();
    tasks
        .set_running("waiting-task", "recover-run")
        .await
        .unwrap();
    joyczl_state::Runs::new(pool.clone())
        .start(
            "recover-run",
            "waiting-task",
            Some("s"),
            Some("recover-run"),
        )
        .await
        .unwrap();
    joyczl_state::Jobs::new(pool.clone())
        .record_started_for_run("job", "s", "background", "output.log", Some("recover-run"))
        .await
        .unwrap();
    joyczl_state::Checkpoints::new(pool.clone())
        .save_round(
            "recover-run",
            "s",
            "work",
            "[]",
            0,
            "{}",
            Some("recover-run"),
            Some("round"),
        )
        .await
        .unwrap();
    JobWaits::new(pool.clone())
        .register(
            "waiting-task",
            "recover-run",
            "recover-run",
            "s",
            &["job".into()],
        )
        .await
        .unwrap();
    let server = Server::boot(
        pool,
        joyczl_config::Settings {
            home: home.path().into(),
            exec_enabled: false,
            ..Default::default()
        },
    )
    .await;
    wait_for_status(&server, joyczl_state::TaskStatus::NeedsAttention).await;
    assert_eq!(
        joyczl_state::Jobs::new(server.pool.clone())
            .get("job")
            .await
            .unwrap()
            .unwrap()
            .status,
        "interrupted"
    );
    assert_eq!(
        server
            .checkpoints
            .get("recover-run")
            .await
            .unwrap()
            .unwrap()
            .status,
        "suspended"
    );
}
