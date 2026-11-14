use crate::{EventSink, Frame, Server};
use joyczl_protocol::{JsonRpcMessage, JsonRpcRequest, RequestId, TurnStartParams};
use joyczl_provider::{CreateRequest, CreateResponse, Provider, ProviderError, Resolved, TextSink};
use serde_json::{json, Value};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Notify;

type Reply<'a> = Pin<Box<dyn Future<Output = Result<CreateResponse, ProviderError>> + Send + 'a>>;
#[derive(Default)]
struct BlockingModel {
    entered: Notify,
    release: Notify,
    requests: Mutex<Vec<String>>,
}
impl Provider for BlockingModel {
    fn create(&self, request: CreateRequest) -> Reply<'_> {
        Box::pin(async move {
            if request.tools.is_empty() {
                return Ok(joyczl_provider::mock::Mock::text(
                    r#"{"retrieve":false,"query":"","reason":"test"}"#,
                ));
            }
            let text = request
                .messages
                .iter()
                .map(|m| m.text())
                .collect::<Vec<_>>()
                .join("\n");
            self.requests.lock().unwrap().push(format!(
                "{}\n{}",
                request.system_text().unwrap_or_default(),
                text
            ));
            if text.contains("private-task-marker") {
                self.entered.notify_one();
                self.release.notified().await;
                Ok(joyczl_provider::mock::Mock::text("Task output produced."))
            } else {
                Ok(joyczl_provider::mock::Mock::text("I am here with you."))
            }
        })
    }
    fn stream(&self, request: CreateRequest, _sink: TextSink) -> Reply<'_> {
        self.create(request)
    }
}
async fn rpc(server: &Server, method: &str, params: Value) -> Result<Value, String> {
    let (sink, mut receiver) = EventSink::channel();
    crate::handle(
        server,
        JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: RequestId::Number(1),
            method: method.into(),
            params: Some(params),
        },
        sink,
    )
    .await;
    while let Some(frame) = receiver.recv().await {
        match frame {
            Frame::Response(JsonRpcMessage::Response(r)) => return Ok(r.result),
            Frame::Response(JsonRpcMessage::Error(r)) => return Err(r.error.message),
            _ => {}
        }
    }
    Err("No response".into())
}
async fn wait_finished(server: &Server, id: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if server
                .tasks
                .get(id)
                .await
                .unwrap()
                .unwrap()
                .status
                .is_terminal()
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn queued_task_and_conversation_run_independently() {
    let home = tempfile::tempdir().unwrap();
    let pool = joyczl_state::open(&home.path().join("state.db"))
        .await
        .unwrap();
    let server = Server::boot(
        pool,
        joyczl_config::Settings {
            home: home.path().into(),
            graph_workflows: false,
            ..Default::default()
        },
    )
    .await;
    let model = Arc::new(BlockingModel::default());
    server.install_provider(Resolved {
        client: model.clone(),
        model: "test".into(),
        small_model: "test".into(),
        provider_id: "mock".into(),
        vision: false,
        context_window: 100_000,
    });
    std::fs::write(home.path().join("contract-proof.txt"), "proof").unwrap();
    let created = rpc(
        &server,
        "task/create",
        json!({"title":"Research a private task","sessionId":"conversation","cwd":home.path(),"acceptance":[{"kind":"fileExists","path":"contract-proof.txt"}],"budget":{"tokens":500}}),
    )
    .await
    .unwrap();
    let id = created["task"]["id"].as_str().unwrap();
    rpc(
        &server,
        "task/start",
        json!({"taskId":id,"message":"private-task-marker"}),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), model.entered.notified())
        .await
        .unwrap();
    assert!(rpc(
        &server,
        "task/start",
        json!({"taskId":id,"message":"duplicate"})
    )
    .await
    .is_err());
    let (sink, mut receiver) = EventSink::channel();
    tokio::time::timeout(
        Duration::from_secs(5),
        crate::run_turn(
            &server,
            TurnStartParams {
                session_id: Some("conversation".into()),
                message: "I am anxious; keep me company.".into(),
                ..Default::default()
            },
            RequestId::Number(3),
            &sink,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        server.tasks.get(id).await.unwrap().unwrap().status,
        joyczl_state::TaskStatus::Running
    );
    let mut reply = None;
    while let Ok(frame) = receiver.try_recv() {
        if let Frame::Notification(joyczl_protocol::ServerNotification::TurnCompleted(done)) = frame
        {
            reply = Some(done.reply);
        }
    }
    assert_eq!(reply.as_deref(), Some("I am here with you."));
    let task = rpc(&server, "task/get", json!({"taskId":id}))
        .await
        .unwrap();
    assert_ne!(task["task"]["executionSessionId"], "conversation");
    assert!(!model
        .requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .contains("private-task-marker"));
    assert!(model
        .requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .contains("Task progress snapshot"));
    model.release.notify_one();
    wait_finished(&server, id).await;
    assert_eq!(server.runs.list_by_task(id, 10).await.unwrap().len(), 1);
    let requests = model.requests.lock().unwrap();
    assert!(requests[0].contains("Durable task handoff"));
    assert!(requests[0].contains("contract-proof.txt"));
    assert!(!requests.last().unwrap().contains("contract-proof.txt"));
}

#[tokio::test]
async fn boot_dispatches_pending_work_without_resubmitting_claimed_work() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("state.db");
    let pool = joyczl_state::open(&path).await.unwrap();
    let tasks = joyczl_state::Tasks::new(pool.clone());
    for id in ["pending-task", "unknown-task"] {
        tasks
            .create(joyczl_state::NewTask {
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
    let queue = joyczl_state::dispatch::TaskDispatch::new(pool.clone());
    queue
        .enqueue("unknown-task", "must not repeat")
        .await
        .unwrap();
    queue.claim().await.unwrap();
    queue
        .enqueue("pending-task", "authorized work")
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
    let mock = Arc::new(joyczl_provider::mock::Mock::new(vec![
        joyczl_provider::mock::Mock::text(r#"{"retrieve":false,"query":"","reason":"test"}"#),
        joyczl_provider::mock::Mock::text("Produced."),
    ]));
    server.install_provider(Resolved {
        client: mock.clone(),
        model: "test".into(),
        small_model: "test".into(),
        provider_id: "mock".into(),
        vision: false,
        context_window: 100_000,
    });
    wait_finished(&server, "pending-task").await;
    assert_eq!(
        server
            .tasks
            .get("unknown-task")
            .await
            .unwrap()
            .unwrap()
            .status,
        joyczl_state::TaskStatus::NeedsAttention
    );
    assert_eq!(
        server
            .runs
            .list_by_task("unknown-task", 10)
            .await
            .unwrap()
            .len(),
        0
    );
    assert_eq!(mock.received.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_queued_pause_survives_until_explicit_resume() {
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
    let model = Arc::new(joyczl_provider::mock::Mock::new(vec![
        joyczl_provider::mock::Mock::text(r#"{"retrieve":false,"query":"","reason":"test"}"#),
        joyczl_provider::mock::Mock::text("Produced."),
    ]));
    server.install_provider(Resolved {
        client: model.clone(),
        model: "test".into(),
        small_model: "test".into(),
        provider_id: "mock".into(),
        vision: false,
        context_window: 100_000,
    });
    let lock = server.session_write_lock("background-task-worker");
    let guard = lock.lock().await;
    let created = rpc(
        &server,
        "task/create",
        json!({"title":"pause before starting","sessionId":"chat"}),
    )
    .await
    .unwrap();
    let id = created["task"]["id"].as_str().unwrap();
    rpc(
        &server,
        "task/start",
        json!({"taskId":id,"message":"authorized work"}),
    )
    .await
    .unwrap();
    rpc(&server, "task/suspend", json!({"taskId":id}))
        .await
        .unwrap();
    assert!(model.received.lock().unwrap().is_empty());
    let resumed = rpc(&server, "task/resume", json!({"taskId":id}))
        .await
        .unwrap();
    assert_eq!(resumed["task"]["status"], "pending");
    drop(guard);
    wait_finished(&server, id).await;
    assert_eq!(server.runs.list_by_task(id, 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn unavailable_task_ledger_stops_before_any_model_request() {
    let home = tempfile::tempdir().unwrap();
    let pool = joyczl_state::open(&home.path().join("state.db"))
        .await
        .unwrap();
    let server = Server::boot(
        pool.clone(),
        joyczl_config::Settings {
            home: home.path().into(),
            ..Default::default()
        },
    )
    .await;
    let mock = Arc::new(joyczl_provider::mock::Mock::new(vec![]));
    server.install_provider(Resolved {
        client: mock.clone(),
        model: "test".into(),
        small_model: "test".into(),
        provider_id: "mock".into(),
        vision: false,
        context_window: 100_000,
    });
    sqlx::query("ALTER TABLE agent_tasks RENAME TO unavailable_tasks")
        .execute(&pool)
        .await
        .unwrap();
    let (sink, _receiver) = EventSink::channel();
    let error = crate::run_turn(
        &server,
        TurnStartParams {
            message: "do work".into(),
            ..Default::default()
        },
        RequestId::Number(1),
        &sink,
    )
    .await
    .unwrap_err();
    assert!(error.message.contains("Cannot read task state"));
    assert!(mock.received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn human_control_wins_while_recovery_waits_for_the_worker() {
    for action in ["task/suspend", "task/cancel"] {
        let home = tempfile::tempdir().unwrap();
        let pool = joyczl_state::open(&home.path().join("state.db"))
            .await
            .unwrap();
        let server = Server::boot(
            pool.clone(),
            joyczl_config::Settings {
                home: home.path().into(),
                ..Default::default()
            },
        )
        .await;
        let mock = Arc::new(joyczl_provider::mock::Mock::new(vec![]));
        server.install_provider(Resolved {
            client: mock.clone(),
            model: "test".into(),
            small_model: "test".into(),
            provider_id: "mock".into(),
            vision: false,
            context_window: 100_000,
        });
        let lock = server.session_write_lock("background-task-worker");
        let guard = lock.lock().await;
        server
            .tasks
            .create(joyczl_state::NewTask {
                id: "recover",
                session_id: Some("chat"),
                title: "recover",
                goal_condition: None,
                acceptance_json: "[]",
                budget_json: "{}",
                cwd: None,
                max_rounds: 1,
                tier: None,
            })
            .await
            .unwrap();
        let queue = joyczl_state::dispatch::TaskDispatch::new(pool);
        let row = queue.enqueue("recover", "work").await.unwrap();
        queue.claim().await.unwrap();
        queue.finish("recover").await.unwrap();
        server
            .runs
            .start(
                &row.turn_id,
                "recover",
                Some(&row.session_id),
                Some(&row.turn_id),
            )
            .await
            .unwrap();
        server
            .tasks
            .set_running("recover", &row.turn_id)
            .await
            .unwrap();
        server
            .checkpoints
            .save_round(
                &row.turn_id,
                &row.session_id,
                "work",
                "[]",
                0,
                "{}",
                Some(&row.turn_id),
                Some("round"),
            )
            .await
            .unwrap();
        let (sink, mut receiver) = EventSink::channel();
        crate::resume::reconcile(&server, &sink).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(frame) = receiver.recv().await {
                if matches!(
                    frame,
                    Frame::Notification(joyczl_protocol::ServerNotification::TurnStarted(_))
                ) {
                    return;
                }
            }
            panic!("Recovery did not enter");
        })
        .await
        .unwrap();
        rpc(&server, action, json!({"taskId":"recover"}))
            .await
            .unwrap();
        drop(guard);
        let _finished = tokio::time::timeout(Duration::from_secs(5), lock.lock())
            .await
            .unwrap();
        assert!(mock.received.lock().unwrap().is_empty());
        let task = server.tasks.get("recover").await.unwrap().unwrap();
        assert_eq!(
            task.status,
            if action == "task/cancel" {
                joyczl_state::TaskStatus::Cancelled
            } else {
                joyczl_state::TaskStatus::Suspended
            }
        );
    }
}
