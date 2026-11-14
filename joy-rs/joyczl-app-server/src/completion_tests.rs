use crate::{EventSink, Server};
use joyczl_protocol::{RequestId, TurnStartParams};
use joyczl_provider::{mock::Mock, Resolved};
use joyczl_tools::notify::{Notifier, NotifyEvent};
use std::sync::{Arc, Mutex};

struct Observer {
    pool: sqlx::SqlitePool,
    task_id: String,
    statuses: Mutex<Vec<String>>,
}
impl Notifier for Observer {
    fn notify(&self, _event: NotifyEvent) {
        let status = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(
                sqlx::query_scalar::<_, String>("SELECT status FROM agent_tasks WHERE id=?")
                    .bind(&self.task_id)
                    .fetch_one(&self.pool),
            )
        })
        .unwrap();
        self.statuses.lock().unwrap().push(status);
    }
    fn delivery_target(&self) -> Option<String> {
        Some("test-target".into())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_completion_notice_observes_the_committed_task_outcome() {
    let home = tempfile::tempdir().unwrap();
    let pool = joyczl_state::open(&home.path().join("state.db"))
        .await
        .unwrap();
    let mut server = Server::boot(
        pool.clone(),
        joyczl_config::Settings {
            home: home.path().into(),
            ..Default::default()
        },
    )
    .await;
    let mock = Arc::new(Mock::new(vec![
        Mock::text(r#"{"retrieve":false,"query":"","reason":"test"}"#),
        Mock::text("Produced."),
        Mock::text(r#"{"ok":true,"impossible":false,"reason":"measured"}"#),
    ]));
    server.install_provider(Resolved {
        client: mock,
        model: "test".into(),
        small_model: "test".into(),
        provider_id: "mock".into(),
        vision: false,
        context_window: 100_000,
    });
    let task = server
        .set_task_goal("chat", Some("finish the authorized work"))
        .await
        .unwrap();
    let observer = Arc::new(Observer {
        pool,
        task_id: task.id,
        statuses: Mutex::new(vec![]),
    });
    server.notifier = Some(observer.clone());
    let (sink, _receiver) = EventSink::channel();
    crate::run_turn(
        &server,
        TurnStartParams {
            session_id: Some("chat".into()),
            message: "do the work".into(),
            ..Default::default()
        },
        RequestId::Number(1),
        &sink,
    )
    .await
    .unwrap();
    let statuses = observer.statuses.lock().unwrap();
    assert_eq!(
        *statuses,
        vec!["done"],
        "No completion may be advertised before its task outcome commits"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_completion_commit_cannot_return_a_successful_turn() {
    let home = tempfile::tempdir().unwrap();
    let pool = joyczl_state::open(&home.path().join("state.db"))
        .await
        .unwrap();
    let mut server = Server::boot(
        pool.clone(),
        joyczl_config::Settings {
            home: home.path().into(),
            ..Default::default()
        },
    )
    .await;
    let mock = Arc::new(Mock::new(vec![
        Mock::text(r#"{"retrieve":false,"query":"","reason":"test"}"#),
        Mock::text("Produced."),
        Mock::text(r#"{"ok":true,"impossible":false,"reason":"measured"}"#),
    ]));
    server.install_provider(Resolved {
        client: mock,
        model: "test".into(),
        small_model: "test".into(),
        provider_id: "mock".into(),
        vision: false,
        context_window: 100_000,
    });
    let task = server
        .set_task_goal("chat", Some("finish the authorized work"))
        .await
        .unwrap();
    let observer = Arc::new(Observer {
        pool: pool.clone(),
        task_id: task.id.clone(),
        statuses: Mutex::new(vec![]),
    });
    server.notifier = Some(observer.clone());
    sqlx::query("CREATE TRIGGER fail_done BEFORE INSERT ON task_completions WHEN NEW.status='done' BEGIN SELECT RAISE(FAIL,'injected completion failure'); END").execute(&pool).await.unwrap();
    let (sink, _receiver) = EventSink::channel();
    let error = crate::run_turn(
        &server,
        TurnStartParams {
            session_id: Some("chat".into()),
            message: "do the work".into(),
            ..Default::default()
        },
        RequestId::Number(1),
        &sink,
    )
    .await
    .unwrap_err();
    assert!(error.message.contains("completion is not confirmed"));
    assert_eq!(
        server.tasks.get(&task.id).await.unwrap().unwrap().status,
        joyczl_state::TaskStatus::NeedsAttention
    );
    assert!(observer.statuses.lock().unwrap().is_empty());
    let notices = server.completions.pending().await.unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].status, "needs_attention");
}

struct QueueSink {
    target: String,
    fail: bool,
    seen: Mutex<Vec<String>>,
}
impl Notifier for QueueSink {
    fn notify(&self, _event: NotifyEvent) {
        panic!("Acknowledged enqueue is required");
    }
    fn delivery_target(&self) -> Option<String> {
        Some(self.target.clone())
    }
    fn enqueue(&self, event: NotifyEvent) -> Result<(), String> {
        if self.fail {
            return Err("Injected enqueue failure".into());
        }
        self.seen.lock().unwrap().push(event.message());
        Ok(())
    }
}
#[tokio::test]
async fn task_delivery_survives_enqueue_failure_and_destination_changes() {
    let home = tempfile::tempdir().unwrap();
    let pool = joyczl_state::open(&home.path().join("state.db"))
        .await
        .unwrap();
    let tasks = joyczl_state::Tasks::new(pool.clone());
    tasks
        .create(joyczl_state::NewTask {
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
    let rows = joyczl_state::completions::Completions::new(pool.clone());
    rows.bind("task", true, Some("original")).await.unwrap();
    tasks
        .finish("task", joyczl_state::TaskStatus::Done)
        .await
        .unwrap();
    crate::completion_delivery::deliver(
        &rows,
        &QueueSink {
            target: "original".into(),
            fail: true,
            seen: Mutex::new(vec![]),
        },
    )
    .await;
    assert_eq!(rows.pending().await.unwrap().len(), 1);
    drop(rows);
    let rows = joyczl_state::completions::Completions::new(pool);
    let changed = QueueSink {
        target: "changed".into(),
        fail: false,
        seen: Mutex::new(vec![]),
    };
    crate::completion_delivery::deliver(&rows, &changed).await;
    assert!(changed.seen.lock().unwrap().is_empty());
    let accepted = QueueSink {
        target: "original".into(),
        fail: false,
        seen: Mutex::new(vec![]),
    };
    crate::completion_delivery::deliver(&rows, &accepted).await;
    crate::completion_delivery::deliver(&rows, &accepted).await;
    assert_eq!(accepted.seen.lock().unwrap().len(), 1);
    assert!(rows.pending().await.unwrap().is_empty());
}

#[tokio::test]
async fn schedule_notification_retries_the_original_committed_destination() {
    let home = tempfile::tempdir().unwrap();
    let pool = joyczl_state::open(&home.path().join("state.db"))
        .await
        .unwrap();
    let schedules = joyczl_state::Schedules::new(pool.clone());
    let rows = joyczl_state::completions::Completions::new(pool);
    schedules.ensure("job", true, "notify").await.unwrap();
    schedules.claim("job", "minute").await.unwrap();
    rows.schedules()
        .settle(joyczl_state::schedule_completions::ScheduleCompletion {
            name: "job",
            minute: "minute",
            status: "needs_attention",
            detail: "Review required",
            output: "",
            preview: "Unknown effects",
            output_path: None,
            notify_requested: true,
            notify_target: Some("original"),
        })
        .await
        .unwrap();
    crate::completion_delivery::deliver(
        &rows,
        &QueueSink {
            target: "original".into(),
            fail: true,
            seen: Mutex::new(vec![]),
        },
    )
    .await;
    assert_eq!(rows.schedules().pending().await.unwrap().len(), 1);
    let changed = QueueSink {
        target: "changed".into(),
        fail: false,
        seen: Mutex::new(vec![]),
    };
    crate::completion_delivery::deliver(&rows, &changed).await;
    assert!(changed.seen.lock().unwrap().is_empty());
    let accepted = QueueSink {
        target: "original".into(),
        fail: false,
        seen: Mutex::new(vec![]),
    };
    crate::completion_delivery::deliver(&rows, &accepted).await;
    crate::completion_delivery::deliver(&rows, &accepted).await;
    assert_eq!(accepted.seen.lock().unwrap().len(), 1);
    assert!(rows.schedules().pending().await.unwrap().is_empty());
}
