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

type Reply<'a> = Pin<Box<dyn Future<Output = Result<CreateResponse, ProviderError>> + Send + 'a>>;
#[derive(Default)]
pub(super) struct InputModel {
    requests: Mutex<Vec<CreateRequest>>,
    write_artifact: std::sync::atomic::AtomicBool,
}
impl Provider for InputModel {
    fn create(&self, request: CreateRequest) -> Reply<'_> {
        Box::pin(async move {
            if request.tools.is_empty() {
                return Ok(joyczl_provider::mock::Mock::text(
                    r#"{"retrieve":false,"query":"","reason":"test"}"#,
                ));
            }
            let system = request.system_text().unwrap_or_default();
            let reply = if system.contains("conditionId") {
                if system.contains(r#""data":{"value":42}"#) {
                    "Final report using 42."
                } else {
                    "Private draft: waiting for the figures."
                }
            } else {
                "I am here with you."
            };
            let write = self.write_artifact.load(std::sync::atomic::Ordering::Relaxed)
                && system.contains(r#""data":{"value":42}"#)
                && !request.messages.iter().any(|message| message.content.iter().any(|block|
                    matches!(block, joyczl_provider::ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "meeting-artifact")));
            self.requests.lock().unwrap().push(request);
            if write {
                Ok(joyczl_provider::mock::Mock::tool_use(
                    "meeting-artifact",
                    "write_file",
                    json!({"path":"meeting.md","content":"Approved meeting figures: 42"}),
                ))
            } else {
                Ok(joyczl_provider::mock::Mock::text(reply))
            }
        })
    }
    fn stream(&self, request: CreateRequest, _sink: TextSink) -> Reply<'_> {
        self.create(request)
    }
}
pub(super) async fn rpc(server: &Server, method: &str, params: Value) -> Result<Value, String> {
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
            Frame::Response(JsonRpcMessage::Response(response)) => return Ok(response.result),
            Frame::Response(JsonRpcMessage::Error(error)) => return Err(error.error.message),
            _ => (),
        }
    }
    Err("No RPC response".into())
}
pub(super) async fn fixture() -> (tempfile::TempDir, Server, Arc<InputModel>) {
    let home = tempfile::tempdir().unwrap();
    let pool = joyczl_state::open(&home.path().join("state.db"))
        .await
        .unwrap();
    let server = Server::boot(
        pool,
        joyczl_config::Settings {
            home: home.path().into(),
            provider: "test-unconfigured".into(),
            write_enabled: true,
            ..Default::default()
        },
    )
    .await;
    let model = Arc::new(InputModel::default());
    server.install_provider(Resolved {
        client: model.clone(),
        model: "test".into(),
        small_model: "test".into(),
        provider_id: "mock".into(),
        vision: false,
        context_window: 100_000,
    });
    server
        .tasks
        .create(joyczl_state::NewTask {
            id: "input-task",
            session_id: Some("chat"),
            title: "prepare meeting report",
            goal_condition: None,
            acceptance_json: "[]",
            budget_json: "{}",
            cwd: None,
            max_rounds: 3,
            tier: None,
        })
        .await
        .unwrap();
    rpc(&server,"task/wait",json!({"taskId":"input-task","source":"approved-host","eventKey":"figures","description":"Wait for the approved meeting figures"})).await.unwrap();
    (home, server, model)
}
pub(super) async fn wait_status(server: &Server, status: joyczl_state::TaskStatus) {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if server
                .tasks
                .get("input-task")
                .await
                .unwrap()
                .unwrap()
                .status
                == status
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
fn signal() -> Value {
    json!({"conditionId":"input-task:input","source":"approved-host","eventKey":"figures","eventId":"event-1","data":{"value":42}})
}

#[tokio::test]
async fn external_wait_keeps_the_draft_allows_chat_and_consumes_one_event() {
    let (_home, server, model) = fixture().await;
    rpc(
        &server,
        "task/start",
        json!({"taskId":"input-task","message":"Prepare the meeting report"}),
    )
    .await
    .unwrap();
    wait_status(&server, joyczl_state::TaskStatus::Suspended).await;
    let waits = joyczl_state::external_waits::ExternalWaits::new(server.pool.clone());
    let wait = waits.waiting("input-task").await.unwrap().unwrap();
    let cp = server
        .checkpoints
        .get(&wait.turn_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cp.status, "waiting_input");
    assert!(cp.messages_json.contains("Private draft"));
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    let (sink, _receiver) = EventSink::channel();
    crate::run_turn(
        &server,
        TurnStartParams {
            session_id: Some("chat".into()),
            message: "I am tired; talk with me".into(),
            ..Default::default()
        },
        RequestId::Number(2),
        &sink,
    )
    .await
    .unwrap();
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    let before = server
        .tasks
        .get("input-task")
        .await
        .unwrap()
        .unwrap()
        .spent_tokens;
    assert_eq!(
        rpc(&server, "task/signal", signal()).await.unwrap()["duplicate"],
        false
    );
    assert_eq!(
        rpc(&server, "task/signal", signal()).await.unwrap()["duplicate"],
        true
    );
    wait_status(&server, joyczl_state::TaskStatus::Done).await;
    let task = server.tasks.get("input-task").await.unwrap().unwrap();
    assert_eq!(task.rounds_used, 1);
    assert!(task.spent_tokens > before);
    assert_eq!(server.runs.count_by_task("input-task").await.unwrap(), 1);
    assert_eq!(
        server.runs.get(&wait.run_id).await.unwrap().unwrap().status,
        "completed"
    );
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(!requests[1]
        .messages
        .iter()
        .any(|message| message.text().contains("Private draft")));
    assert!(requests[2]
        .messages
        .iter()
        .any(|message| message.text().contains("Private draft")));
    assert!(requests[2]
        .messages
        .iter()
        .any(|message| message.text().contains("event-1")));
    assert!(requests[2].system_text().unwrap().contains(r#""value":42"#));
}

#[tokio::test]
async fn input_received_during_human_pause_waits_for_explicit_resume() {
    let (_home, server, model) = fixture().await;
    rpc(
        &server,
        "task/start",
        json!({"taskId":"input-task","message":"Prepare the report"}),
    )
    .await
    .unwrap();
    wait_status(&server, joyczl_state::TaskStatus::Suspended).await;
    rpc(
        &server,
        "task/suspend",
        json!({"taskId":"input-task","reason":"Human pause"}),
    )
    .await
    .unwrap();
    rpc(&server, "task/signal", signal()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    assert_eq!(
        server
            .tasks
            .get("input-task")
            .await
            .unwrap()
            .unwrap()
            .suspend_reason
            .as_deref(),
        Some("Human pause")
    );
    rpc(&server, "task/resume", json!({"taskId":"input-task"}))
        .await
        .unwrap();
    wait_status(&server, joyczl_state::TaskStatus::Done).await;
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1]
        .messages
        .iter()
        .any(|message| message.text().contains("event-1")));
}

#[tokio::test]
async fn a_received_input_survives_restart_and_resumes_the_original_draft() {
    use joyczl_state::external_waits::{ExternalWaits, InputWait, WaitSnapshot};
    let (home, old_server, _old_model) = fixture().await;
    let pool = old_server.pool.clone();
    old_server
        .runs
        .start(
            "restart-run",
            "input-task",
            Some("restart-work"),
            Some("restart-turn"),
        )
        .await
        .unwrap();
    old_server
        .tasks
        .set_running("input-task", "restart-turn")
        .await
        .unwrap();
    let messages = serde_json::to_string(&vec![
        joyczl_provider::Message::user_text("Prepare the report"),
        joyczl_provider::Message {
            role: joyczl_provider::Role::Assistant,
            content: vec![joyczl_provider::ContentBlock::Text {
                text: "Private draft: waiting for the figures.".into(),
            }],
        },
    ])
    .unwrap();
    let usage = r#"{"input_tokens":10,"output_tokens":5}"#;
    old_server
        .checkpoints
        .save_numbered_round(joyczl_state::checkpoints::CheckpointWrite {
            turn_id: "restart-turn",
            session_id: "restart-work",
            user_message: "Prepare the report",
            messages_json: &messages,
            iterations: 1,
            usage_json: usage,
            run_id: Some("restart-run"),
            round_id: Some("restart-round"),
            round_no: Some(1),
        })
        .await
        .unwrap();
    old_server
        .runs
        .account_round(
            "restart-run",
            "restart-round",
            &joyczl_state::runs::RoundAccount {
                tokens_in: 10,
                tokens_out: 5,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let rows = ExternalWaits::new(pool.clone());
    rows.register(
        &InputWait {
            task_id: "input-task".into(),
            condition_id: "input-task:input".into(),
            run_id: "restart-run".into(),
            turn_id: "restart-turn".into(),
            session_id: "restart-work".into(),
            state: "waiting".into(),
        },
        WaitSnapshot {
            messages_json: &messages,
            usage_json: usage,
            iterations: 1,
            round_id: "restart-round",
            round_no: 1,
        },
    )
    .await
    .unwrap();
    rows.signal(
        "input-task:input",
        "approved-host",
        "figures",
        "event-1",
        r#"{"value":42}"#,
    )
    .await
    .unwrap();
    drop(old_server);
    let server = Server::boot(
        pool,
        joyczl_config::Settings {
            home: home.path().into(),
            provider: "test-unconfigured".into(),
            ..Default::default()
        },
    )
    .await;
    let model = Arc::new(InputModel::default());
    server.install_provider(Resolved {
        client: model.clone(),
        model: "test".into(),
        small_model: "test".into(),
        provider_id: "mock".into(),
        vision: false,
        context_window: 100_000,
    });
    wait_status(&server, joyczl_state::TaskStatus::Done).await;
    assert_eq!(server.runs.count_by_task("input-task").await.unwrap(), 1);
    assert_eq!(
        server
            .runs
            .get("restart-run")
            .await
            .unwrap()
            .unwrap()
            .status,
        "completed"
    );
    assert_eq!(
        server
            .tasks
            .get("input-task")
            .await
            .unwrap()
            .unwrap()
            .rounds_used,
        1
    );
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0]
        .messages
        .iter()
        .any(|message| message.text().contains("Private draft")));
    assert!(requests[0]
        .system_text()
        .unwrap()
        .contains(r#""runId":"restart-run""#));
}

#[tokio::test]
async fn received_input_produces_a_verified_file_in_the_original_workspace() {
    let (home, server, model) = fixture().await;
    model
        .write_artifact
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let acceptance = json!([
        {"kind":"fileContains","path":"meeting.md","pattern":"42","min":1},
        {"kind":"toolsOk","tool":"write_file","min":1}
    ]);
    sqlx::query("UPDATE agent_tasks SET cwd=?,acceptance_json=? WHERE id='input-task'")
        .bind(home.path().display().to_string())
        .bind(acceptance.to_string())
        .execute(&server.pool)
        .await
        .unwrap();
    rpc(
        &server,
        "task/start",
        json!({"taskId":"input-task","message":"Prepare meeting.md using the approved figures"}),
    )
    .await
    .unwrap();
    wait_status(&server, joyczl_state::TaskStatus::Suspended).await;
    assert!(!home.path().join("meeting.md").exists());
    rpc(&server, "task/signal", signal()).await.unwrap();
    wait_status(&server, joyczl_state::TaskStatus::Done).await;
    assert_eq!(
        std::fs::read_to_string(home.path().join("meeting.md")).unwrap(),
        "Approved meeting figures: 42"
    );
    let task = server.tasks.get("input-task").await.unwrap().unwrap();
    let checks: Vec<Value> = serde_json::from_str(&task.checks_json).unwrap();
    assert_eq!(checks.len(), 2);
    assert!(checks.iter().all(|check| check["passed"] == true));
    assert_eq!(task.rounds_used, 1);
    assert_eq!(server.runs.count_by_task("input-task").await.unwrap(), 1);
}
