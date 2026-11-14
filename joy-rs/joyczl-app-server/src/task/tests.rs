//! Long-task entity tests — one question per test, all through the real dispatch path:
//! does the task row tell the truth about what the human asked for?
//!
//! The interesting half is the pause/resume pair: pausing must **keep** the checkpoint
//! (a pause is not a stop), and continuing must resume it instead of replaying tools.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use joyczl_config::Settings;
use joyczl_protocol::{
    codes, JsonRpcMessage, JsonRpcRequest, RequestId, ServerNotification, TaskEnvelope,
    TurnStartParams,
};
use joyczl_provider::mock::Mock;
use joyczl_provider::{Message, Resolved, Role};
use serde_json::json;
use tokio::sync::mpsc;

use crate::{EventSink, Frame, Server};

/// A server plus its home — booting a second one on the same home is how "restart" is tested.
async fn boot(mock: Option<Arc<Mock>>) -> (Server, PathBuf) {
    boot_with(mock, false).await
}

/// Same boot, with the planner switch (`JOY_PLAN`) set explicitly — planning is opt-in.
async fn boot_with(mock: Option<Arc<Mock>>, plan_enabled: bool) -> (Server, PathBuf) {
    let dir = tempfile::tempdir().expect("temporary directory");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("open the store");
    let _ = dir.keep();
    let server = Server::boot(
        pool,
        Settings {
            home: home.clone(),
            // Graph routing would spend extra model calls on triage; these tests count
            // turns, not routes — the direct path is the one under test.
            graph_workflows: false,
            // Acceptance conditions are about files that really exist, so the write tools must
            // be registered: without them the "fake completion" tests would pass for the wrong
            // reason (no tool at all, instead of a tool that did not write).
            write_enabled: true,
            plan_enabled,
            ..Settings::default()
        },
    )
    .await;
    if let Some(mock) = mock {
        server.set_resolved(Some(Resolved {
            context_window: joyczl_provider::DEFAULT_CONTEXT_WINDOW,
            vision: false,
            provider_id: "mock".to_string(),
            client: mock,
            model: "test-model".to_string(),
            small_model: "test-small".to_string(),
        }));
    }
    (server, home)
}

/// Send one request and hand back the decoded result (or the error message).
async fn call(
    server: &Server,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let request = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: RequestId::Number(1),
        method: method.to_string(),
        params: Some(params),
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    crate::handle(server, request, EventSink::new(tx)).await;
    let mut response = None;
    while let Ok(frame) = rx.try_recv() {
        if let Frame::Response(message) = frame {
            response = Some(message);
        }
    }
    match response.expect("every request has a reply") {
        JsonRpcMessage::Response(response) => Ok(response.result),
        JsonRpcMessage::Error(error) => Err(error.error.message),
    }
}

async fn create_task(
    server: &Server,
    session: &str,
    title: &str,
    goal: Option<&str>,
) -> TaskEnvelope {
    let result = call(
        server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({ "sessionId": session, "title": title, "goalCondition": goal }),
    )
    .await
    .expect("creating the task should succeed");
    serde_json::from_value(result).expect("TaskEnvelope")
}

#[tokio::test]
async fn several_tasks_may_be_open_in_one_session_and_the_newest_is_the_card() {
    let (server, _home) = boot(None).await;
    let first = create_task(
        &server,
        "default",
        "watch the server for three days",
        Some("watch it all three days"),
    )
    .await;

    // 0.14 removed "one active task per session": two intents can genuinely coexist, and the
    // row used to be the only place a task could live.
    let second = create_task(&server, "default", "another thing", None).await;
    assert_ne!(first.task.id, second.task.id);

    let open = server
        .tasks
        .list_active()
        .await
        .expect("list the active tasks");
    assert_eq!(open.len(), 2, "both are still there");

    // The card shows one: the newest. `task/list` shows both.
    let view = call(
        &server,
        joyczl_protocol::methods::TASK_VIEW,
        json!({ "sessionId": "default" }),
    )
    .await
    .expect("the board should answer");
    assert_eq!(
        view["task"]["title"],
        json!("another thing"),
        "the card shows the newest one"
    );

    // A different session is a different scope, as always.
    create_task(&server, "other", "the third thing", None).await;
}

/// A task may declare its tier ("the task picks the model"). The declaration lands on the row —
/// every round runs on it — and a typo is refused at creation, not discovered on a vendor bill.
#[tokio::test]
async fn a_task_can_declare_its_tier_and_a_typo_is_refused() {
    let (server, _home) = boot(None).await;

    let error = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({ "sessionId": "default", "title": "read it through", "tier": "reasoining" }),
    )
    .await
    .expect_err("a misspelt tier must be refused on the spot");
    assert!(
        error.contains("reasoining"),
        "the error has to carry the string it received: {error}"
    );

    let created = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({ "sessionId": "default", "title": "read it through", "tier": "reasoning" }),
    )
    .await
    .expect("a legal tier should create the task");
    assert_eq!(created["task"]["tier"], json!("reasoning"));

    let id = created["task"]["id"].as_str().expect("it has an id");
    let row = server
        .tasks
        .get(id)
        .await
        .expect("query")
        .expect("the task is there");
    assert_eq!(
        row.tier.as_deref(),
        Some("reasoning"),
        "the tier lands on the task row, not on one round"
    );

    // No declaration = no field on the wire; never a made-up "chat".
    let (plain_server, _plain_home) = boot(None).await;
    let plain = create_task(&plain_server, "default", "a casual question", None).await;
    assert!(plain.task.tier.is_none());
}

#[tokio::test]
async fn setting_a_goal_creates_the_task_and_clearing_keeps_the_row() {
    let (server, _home) = boot(None).await;
    let row = server
        .set_task_goal("default", Some("get the tests green"))
        .await
        .expect("setting a goal should create the task");

    // There is no separate goal record any more: the condition is a column on this row.
    assert_eq!(row.goal_condition.as_deref(), Some("get the tests green"));
    assert_eq!(row.title, "get the tests green");

    // Clearing the goal downgrades the task to a single-turn one — the row stays as record.
    server
        .set_task_goal("default", None)
        .await
        .expect("clearing the goal should succeed");
    let task = server
        .tasks
        .get(&row.id)
        .await
        .expect("query")
        .expect("still there");
    assert!(task.goal_condition.is_none(), "the goal is cleared");
    assert_eq!(
        task.status,
        joyczl_state::TaskStatus::Pending,
        "the task itself stays"
    );
}

#[tokio::test]
async fn a_single_turn_task_closes_done_when_its_turn_ends() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text("done"),
        Mock::text("done"),
        Mock::text("done"),
    ]));
    let (server, _home) = boot(Some(mock)).await;
    let task = create_task(&server, "default", "check the logs", None).await;
    assert_eq!(task.task.status, "pending");

    // One ordinary turn in that session — it carries the task, and closes it honestly.
    let (tx, _rx) = mpsc::unbounded_channel();
    crate::turn::run_turn(
        &server,
        TurnStartParams {
            attachments: None,
            session_id: Some("default".to_string()),
            message: "let us start".to_string(),
            stream: Some(true),
            cwd: None,
            tier: None,
        },
        RequestId::Number(1),
        &EventSink::new(tx),
    )
    .await
    .expect("the turn should finish");

    let row = server
        .tasks
        .get(&task.task.id)
        .await
        .expect("query")
        .expect("still there");
    assert_eq!(
        row.status,
        joyczl_state::TaskStatus::Done,
        "a single-round task: done the moment the turn ends"
    );
    assert!(row.finished_at.is_some());
    assert!(row.current_turn_id.is_none(), "the heartbeat is cleared");
}

#[tokio::test]
async fn suspending_keeps_the_checkpoint_and_resuming_continues_it() {
    let mock = Arc::new(Mock::new(vec![Mock::text("carried on and finished")]));
    let (server, _home) = boot(Some(mock)).await;
    let task = create_task(
        &server,
        "default",
        "watch the server for three days",
        Some("watch it for all three days"),
    )
    .await;

    server
        .runs
        .start("t-pause", &task.task.id, Some("default"), Some("t-pause"))
        .await
        .expect("run");
    // Plant the paused turn exactly as a real pause leaves it: a checkpoint with working memory.
    let messages = vec![
        Message::user_text("watch the server for me for three days"),
        Message {
            role: Role::Assistant,
            content: vec![joyczl_provider::ContentBlock::Text {
                text: "note it down first".to_string(),
            }],
        },
    ];
    server
        .checkpoints
        .save(
            "t-pause",
            "default",
            "watch the server for me for three days",
            &serde_json::to_string(&messages).expect("serialize"),
            1,
            "{}",
            Some("t-pause"),
        )
        .await
        .expect("write the checkpoint");
    server
        .tasks
        .set_running(&task.task.id, "t-pause")
        .await
        .expect("mark it running");
    // What a real pause leaves behind: the turn's finalize marks the checkpoint `suspended`
    // (the pause flag was set before the interrupt). Simulate that half honestly.
    server
        .checkpoints
        .finish("t-pause", "suspended")
        .await
        .expect("simulate the suspend half of finalize");

    let suspended = call(
        &server,
        joyczl_protocol::methods::TASK_SUSPEND,
        json!({ "taskId": task.task.id }),
    )
    .await
    .expect("suspending should succeed");
    assert_eq!(suspended["task"]["status"], json!("suspended"));
    assert!(
        suspended["task"]["suspendReason"]
            .as_str()
            .is_some_and(|r| !r.is_empty()),
        "the suspension reason has to be recorded"
    );
    let checkpoint = server
        .checkpoints
        .get("t-pause")
        .await
        .expect("query")
        .expect("the checkpoint is still there");
    assert_eq!(
        checkpoint.status, "suspended",
        "a suspension keeps the checkpoint, not marked abandoned"
    );
    assert_eq!(
        server
            .tasks
            .get(&task.task.id)
            .await
            .expect("query")
            .expect("present")
            .current_turn_id
            .as_deref(),
        Some("t-pause"),
        "a suspended task remembers which round it stopped at"
    );

    let resumed = call(
        &server,
        joyczl_protocol::methods::TASK_RESUME,
        json!({ "taskId": task.task.id }),
    )
    .await
    .expect("resuming should succeed");
    assert_eq!(resumed["task"]["status"], json!("running"));

    // The resume runs as its own task. Wait for the **reply**, not the checkpoint: finalize marks
    // the checkpoint `done` before it writes the chat row, so waiting on the checkpoint and then
    // reading history is a race (green alone, red under a loaded parallel test run).
    let mut replied = false;
    for _ in 0..200 {
        replied = server
            .chat
            .session_history("default")
            .await
            .expect("read the conversation log")
            .iter()
            .any(|(_, content)| content.contains("carried on and finished"));
        if replied {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(replied, "the resumed reply reached the ledger");

    let checkpoint = server
        .checkpoints
        .get("t-pause")
        .await
        .expect("query")
        .expect("present");
    assert_eq!(checkpoint.status, "done", "the resume ran to the end");
}

#[tokio::test]
async fn cancelling_closes_the_task_and_never_auto_continues() {
    let (server, _home) = boot(None).await;
    let task = create_task(
        &server,
        "default",
        "the thing that was stopped",
        Some("cannot finish it"),
    )
    .await;
    server
        .checkpoints
        .save(
            "t-stop",
            "default",
            "the thing that was stopped",
            "[]",
            1,
            "{}",
            None,
        )
        .await
        .expect("write the checkpoint");
    server
        .tasks
        .set_running(&task.task.id, "t-stop")
        .await
        .expect("mark it running");

    let cancelled = call(
        &server,
        joyczl_protocol::methods::TASK_CANCEL,
        json!({ "taskId": task.task.id }),
    )
    .await
    .expect("cancelling should succeed");
    assert_eq!(cancelled["task"]["status"], json!("cancelled"));

    let checkpoint = server
        .checkpoints
        .get("t-stop")
        .await
        .expect("query")
        .expect("present");
    assert_eq!(
        checkpoint.status, "abandoned",
        "a cancelled turn is never resumed automatically"
    );
    assert!(
        server
            .checkpoints
            .open_rows()
            .await
            .expect("read")
            .is_empty(),
        "no leftovers to resume"
    );
    assert!(
        server
            .tasks
            .active("default")
            .await
            .expect("query")
            .is_none(),
        "the session is free, so a new task can start"
    );
}

#[tokio::test]
async fn a_restart_marks_a_running_task_suspended_honestly() {
    let (server, home) = boot(None).await;
    let task = create_task(
        &server,
        "default",
        "run a long task",
        Some("until it is done"),
    )
    .await;
    server
        .tasks
        .set_running(&task.task.id, "t-live")
        .await
        .expect("mark it running");
    drop(server); // the process "dies" with the task running

    // Same home, fresh process: boot reconcile must not pretend it is still running.
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("reopen the store");
    let rebooted = Server::boot(
        pool,
        Settings {
            home: home.clone(),
            graph_workflows: false,
            ..Settings::default()
        },
    )
    .await;
    let row = rebooted
        .tasks
        .get(&task.task.id)
        .await
        .expect("query")
        .expect("still there");
    assert_eq!(row.status, joyczl_state::TaskStatus::Suspended);
    assert!(
        row.suspend_reason
            .as_deref()
            .is_some_and(|r| r.contains("the process restarted")),
        "the reason has to be stated plainly: {:?}",
        row.suspend_reason
    );
}

#[tokio::test]
async fn an_unknown_task_id_is_an_error_not_a_panic() {
    let (server, _home) = boot(None).await;
    let error = call(
        &server,
        joyczl_protocol::methods::TASK_RESUME,
        json!({ "taskId": "task-nope" }),
    )
    .await
    .expect_err("a task that does not exist must error");
    assert!(error.contains("no such task"), "{error}");

    // And a task that already finished cannot be resumed either.
    let task = create_task(&server, "default", "completed", None).await;
    server
        .tasks
        .finish(&task.task.id, joyczl_state::TaskStatus::Done)
        .await
        .expect("finish it");
    let error = call(
        &server,
        joyczl_protocol::methods::TASK_RESUME,
        json!({ "taskId": task.task.id }),
    )
    .await
    .expect_err("a finished task cannot be resumed");
    assert_eq!(codes::INVALID_PARAMS, -32602);
    assert!(error.contains("suspended"), "{error}");
}

/// The task pings on the event bus carry the state, so another device's card can update
/// without guessing (kind is `task`, the app-server's `TaskUpdated` notification is the source).
#[tokio::test]
async fn task_transitions_are_announced() {
    let (server, _home) = boot(None).await;
    let task = create_task(&server, "default", "a task that has to be broadcast", None).await;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let sink = EventSink::new(tx);
    let params = joyczl_protocol::TaskActionParams {
        task_id: task.task.id.clone(),
        reason: Some("paused for the test".to_string()),
    };
    crate::task::suspend(&server, &sink, params)
        .await
        .expect("suspend it");

    let mut announced = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        if let Frame::Notification(ServerNotification::TaskUpdated(updated)) = frame {
            announced.push(updated);
        }
    }
    assert!(
        !announced.is_empty(),
        "a status change has to be broadcast, not kept quiet"
    );
    assert_eq!(announced.last().expect("present").status, "suspended");
    assert_eq!(announced.last().expect("present").task_id, task.task.id);
}

// ---- 0.14: a task exists on its own, and completion is decided by facts -------------------

/// Drive one turn in `session`, with an optional working directory.
async fn drive(server: &Server, session: &str, message: &str, cwd: Option<&str>) {
    let (tx, _rx) = mpsc::unbounded_channel();
    crate::turn::run_turn(
        server,
        TurnStartParams {
            attachments: None,
            session_id: Some(session.to_string()),
            message: message.to_string(),
            stream: Some(true),
            cwd: cwd.map(str::to_string),
            tier: None,
        },
        RequestId::Number(1),
        &EventSink::new(tx),
    )
    .await
    .expect("the turn should finish");
}

/// A task no longer needs a conversation: "do this thing" is a complete sentence.
#[tokio::test]
async fn a_task_can_exist_without_any_session() {
    let (server, _home) = boot(None).await;
    let created = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({ "title": "tidy up the downloads directory", "acceptance": [
            { "kind": "fileExists", "path": "index.md" }
        ] }),
    )
    .await
    .expect("a task with no session should still be created");

    assert!(
        created["task"]["sessionId"].is_null(),
        "no session means no session"
    );
    let id = created["task"]["id"].as_str().expect("it has an id");
    assert_eq!(created["task"]["status"], json!("pending"));
    // The condition is normalised at creation, so the card and the checker always agree.
    assert_eq!(
        created["task"]["acceptance"][0]["label"],
        json!("index.md exists")
    );
    assert_eq!(created["task"]["acceptance"][0]["min"], json!(1));

    // The only way in for such a task: by id.
    let got = call(
        &server,
        joyczl_protocol::methods::TASK_GET,
        json!({ "taskId": id }),
    )
    .await
    .expect("looking it up by id should find it");
    assert_eq!(
        got["task"]["title"],
        json!("tidy up the downloads directory")
    );

    let listed = call(&server, joyczl_protocol::methods::TASK_LIST, json!({}))
        .await
        .expect("the list should succeed");
    assert_eq!(listed["tasks"].as_array().expect("an array").len(), 1);
}

/// The model says "done", the file is not there, the rounds are gone: **needs a human**.
/// Not `done` (the lie would stick), not `failed` (the work may still be finishable).
#[tokio::test]
async fn a_task_whose_acceptance_fails_needs_a_human_not_a_done() {
    // One response per model call: the retrieval gate speaks first (fail-open on plain text),
    // then the turn's own reply.
    let mock = Arc::new(Mock::new(vec![
        Mock::text("skip"),
        Mock::text("the report is written, please take a look."),
    ]));
    let (server, _home) = boot(Some(mock)).await;
    let dir = tempfile::tempdir().expect("temporary directory");
    let workspace = dir.path().display().to_string();

    let created = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({
            "sessionId": "default",
            "title": "write a report",
            "maxRounds": 1,
            "cwd": workspace,
            "acceptance": [{ "kind": "fileExists", "path": "report.md" }],
        }),
    )
    .await
    .expect("create the task");
    let id = created["task"]["id"]
        .as_str()
        .expect("it has an id")
        .to_string();

    drive(&server, "default", "let us start", None).await;

    let row = server
        .tasks
        .get(&id)
        .await
        .expect("query")
        .expect("still there");
    assert_eq!(
        row.status,
        joyczl_state::TaskStatus::NeedsAttention,
        "acceptance not met means it cannot count as done: {:?}",
        row.last_stop_reason
    );
    let reason = row
        .last_stop_reason
        .as_deref()
        .expect("it has to say why it stopped");
    assert!(reason.contains("report.md does not exist"), "{reason}");

    // The card shows the measurement per condition, not a vague "failed".
    let checks: serde_json::Value =
        serde_json::from_str(&row.checks_json).expect("the acceptance results");
    assert_eq!(checks[0]["passed"], json!(false));
    assert_eq!(checks[0]["label"], json!("report.md exists"));
}

/// The positive twin: a real write makes the same condition pass — and the artifact list
/// records the file that is actually on disk.
#[tokio::test]
async fn a_condition_passes_only_after_the_file_really_exists() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text("skip"),
        Mock::tool_use(
            "call_1",
            "write_file",
            json!({ "path": "report.md", "content": "verdict: ready to ship\n" }),
        ),
        Mock::text("written."),
    ]));
    let (server, _home) = boot(Some(mock)).await;
    let dir = tempfile::tempdir().expect("temporary directory");
    let workspace = dir.path().display().to_string();

    let created = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({
            "sessionId": "default",
            "title": "write a report",
            "cwd": workspace,
            "acceptance": [
                { "kind": "fileExists", "path": "report.md" },
                { "kind": "fileContains", "path": "report.md", "pattern": "verdict", "min": 1 }
            ],
        }),
    )
    .await
    .expect("create the task");
    let id = created["task"]["id"]
        .as_str()
        .expect("it has an id")
        .to_string();

    drive(&server, "default", "let us start", None).await;

    let row = server
        .tasks
        .get(&id)
        .await
        .expect("query")
        .expect("still there");
    assert_eq!(
        row.status,
        joyczl_state::TaskStatus::Done,
        "the file really is there, so it should complete: {:?}",
        row.last_stop_reason
    );
    assert!(row.finished_at.is_some());

    // Deliverables are files that exist, with their real size.
    let artifacts = server
        .artifacts
        .list(&id, 10)
        .await
        .expect("list the deliverables");
    assert_eq!(artifacts.len(), 1);
    assert!(artifacts[0].path.ends_with("report.md"));
    assert!(artifacts[0].bytes > 0);

    // One tool call = one step, keyed so a re-observation cannot duplicate it.
    let runs = server
        .runs
        .list_by_task(&id, 10)
        .await
        .expect("list the runs");
    assert_eq!(runs.len(), 1, "one turn is one run");
    assert_eq!(runs[0].status, "completed");
    assert!(runs[0].tools_ok >= 1, "successful calls are recorded");

    let steps = server
        .steps
        .list_by_run(&runs[0].id)
        .await
        .expect("list the steps");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].tool.as_deref(), Some("write_file"));
    assert_eq!(steps[0].status, "ok");
    let key = steps[0]
        .idempotency_key
        .as_deref()
        .expect("it has an idempotency key");
    assert!(
        key.ends_with(":1"),
        "the key carries the step number: {key}"
    );
    assert!(
        key.starts_with(&runs[0].id),
        "the key carries the turn: {key}"
    );
}

/// A cap that is reached stops the task **before** another round can spend: the state is
/// `needs_attention` (a human may raise the cap), and the reason names the dimension.
#[tokio::test]
async fn a_task_over_budget_stops_before_it_spends_more() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text("skip"),
        Mock::text("do a little first."),
        Mock::text("do a little more."),
    ]));
    let (server, _home) = boot(Some(mock)).await;

    let created = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({
            "sessionId": "default",
            "title": "take it slowly",
            "budget": { "tokens": 10 },
        }),
    )
    .await
    .expect("create the task");
    let id = created["task"]["id"]
        .as_str()
        .expect("it has an id")
        .to_string();

    drive(&server, "default", "let us start", None).await;

    let row = server
        .tasks
        .get(&id)
        .await
        .expect("query")
        .expect("still there");
    assert_eq!(row.status, joyczl_state::TaskStatus::NeedsAttention);
    assert!(
        row.spent_tokens >= 10,
        "the spend has to be recorded: {}",
        row.spent_tokens
    );
    let reason = row
        .last_stop_reason
        .as_deref()
        .expect("it has to say which limit was hit");
    assert!(
        reason.contains("Over budget") && reason.contains("tokens"),
        "{reason}"
    );
}

/// Every turn that carries the task is its own execution: the card's "run N times" is a count of
/// rows, not of rounds.
#[tokio::test]
async fn each_turn_that_carries_the_task_is_its_own_run() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text("skip"),
        Mock::text("first pass."),
        Mock::text("skip"),
        Mock::text("second pass."),
    ]));
    let (server, _home) = boot(Some(mock)).await;
    let dir = tempfile::tempdir().expect("temporary directory");
    let workspace = dir.path().display().to_string();

    // `needs_attention` is open on purpose: a human may look at the failed condition and just
    // ask again — that is what makes a second execution possible at all.
    let created = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({
            "sessionId": "default",
            "title": "do it in several goes",
            "maxRounds": 1,
            "cwd": workspace,
            "acceptance": [{ "kind": "fileExists", "path": "report.md" }],
        }),
    )
    .await
    .expect("create the task");
    let id = created["task"]["id"]
        .as_str()
        .expect("it has an id")
        .to_string();

    drive(&server, "default", "step one", None).await;
    assert_eq!(
        server
            .runs
            .count_by_task(&id)
            .await
            .expect("count the runs"),
        1,
        "the first pass is one run"
    );
    let row = server
        .tasks
        .get(&id)
        .await
        .expect("query")
        .expect("still there");
    assert_eq!(row.status, joyczl_state::TaskStatus::NeedsAttention);
    assert!(
        row.current_turn_id.is_none(),
        "the heartbeat has to be cleared"
    );

    // The second turn picks the same task up again — a new run row, same task.
    drive(&server, "default", "step two", None).await;
    assert_eq!(
        server
            .runs
            .count_by_task(&id)
            .await
            .expect("count the runs"),
        2,
        "the second pass is a new run"
    );

    // Both attempts are readable, newest first.
    let runs = server
        .runs
        .list_by_task(&id, 10)
        .await
        .expect("list the runs");
    assert_eq!(runs.len(), 2);
    assert_eq!(
        runs[0].status, "waiting",
        "stopped at the acceptance check, waiting for a human"
    );
    assert!(runs[0].stop_reason.is_some());
}

/// Rounds inside one turn share a step numbering, not a step **key**: `<turn_id>:<seq>` has to
/// stay unique, or round 2's tool call collides with round 1's row and is silently dropped —
/// which reads as "the tool never ran" and leaves `toolsOk` counting one success forever.
#[tokio::test]
async fn each_round_keeps_its_own_step_so_tools_ok_counts_them_all() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text("skip"),
        Mock::tool_use(
            "call_1",
            "write_file",
            json!({ "path": "a.md", "content": "A\n" }),
        ),
        Mock::text("the first one."),
        Mock::text("skip"),
        Mock::tool_use(
            "call_2",
            "write_file",
            json!({ "path": "b.md", "content": "B\n" }),
        ),
        Mock::text("both batches are done."),
    ]));
    let (server, _home) = boot(Some(mock)).await;
    let dir = tempfile::tempdir().expect("temporary directory");
    let workspace = dir.path().display().to_string();

    let created = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({
            "sessionId": "default",
            "title": "two rounds, two files written",
            "cwd": workspace,
            "acceptance": [{ "kind": "toolsOk", "tool": "write_file", "min": 2 }],
        }),
    )
    .await
    .expect("create the task");
    let id = created["task"]["id"]
        .as_str()
        .expect("it has an id")
        .to_string();

    drive(&server, "default", "let us start", None).await;

    let row = server
        .tasks
        .get(&id)
        .await
        .expect("query")
        .expect("still there");
    assert_eq!(
        row.status,
        joyczl_state::TaskStatus::Done,
        "two real successful writes should complete it: {:?}",
        row.last_stop_reason
    );
    assert_eq!(
        row.rounds_used, 2,
        "the result comes from two rounds, not one"
    );

    let runs = server
        .runs
        .list_by_task(&id, 10)
        .await
        .expect("list the runs");
    let steps = server
        .steps
        .list_by_run(&runs[0].id)
        .await
        .expect("list the steps");
    assert_eq!(
        steps.len(),
        2,
        "one per round; the idempotency key swallowed neither"
    );
    let mut keys: Vec<&str> = steps
        .iter()
        .filter_map(|step| step.idempotency_key.as_deref())
        .collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(
        keys.len(),
        2,
        "the idempotency keys have to differ: {keys:?}"
    );
}

/// A stop is the human's: interrupting one round must end the task loop **right there**.
///
/// Before this, the interrupted round's acceptance failed and "keep trying" bought another
/// round — the user's stop only took effect once the round cap ran out. The case file gives the
/// mock exactly two replies: if a second round were started, the turn would fail on an empty
/// queue instead of coming back suspended.
#[tokio::test]
async fn an_interrupted_round_suspends_the_task_instead_of_spending_another() {
    let mock = Arc::new(Mock::streaming(vec![
        Mock::text("skip"),
        Mock::text("let me get the shape of it straight first, then I will start writing this report, and there is a lot more to say."),
    ]));
    let (server, _home) = boot(Some(mock)).await;
    let dir = tempfile::tempdir().expect("temporary directory");
    let workspace = dir.path().display().to_string();

    let created = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({
            "sessionId": "default",
            "title": "write a report",
            "maxRounds": 4,
            "cwd": workspace,
            "acceptance": [{ "kind": "fileExists", "path": "report.md" }],
        }),
    )
    .await
    .expect("create the task");
    let id = created["task"]["id"]
        .as_str()
        .expect("it has an id")
        .to_string();

    let (tx, mut rx) = mpsc::unbounded_channel();
    let sink = EventSink::new(tx);
    let runner = server.clone();
    let turn = tokio::spawn(async move {
        crate::turn::run_turn(
            &runner,
            TurnStartParams {
                attachments: None,
                session_id: Some("default".to_string()),
                message: "let us start".to_string(),
                stream: Some(true),
                cwd: None,
                tier: None,
            },
            RequestId::Number(1),
            &sink,
        )
        .await
    });

    let mut deltas = 0usize;
    let mut turn_id = String::new();
    while let Some(frame) = rx.recv().await {
        let Frame::Notification(notification) = frame else {
            continue;
        };
        match notification {
            ServerNotification::TurnStarted(started) => turn_id = started.turn_id,
            ServerNotification::TextDelta(_) => {
                deltas += 1;
                if deltas == 2 {
                    assert!(
                        server.interrupt_turn(&turn_id),
                        "the interrupt should hit the running turn"
                    );
                }
            }
            ServerNotification::TurnCompleted(_) => break,
            _ => {}
        }
    }
    turn.await
        .expect("the turn task should not crash")
        .expect("an interrupt is an ordinary ending, not an error");

    let row = server
        .tasks
        .get(&id)
        .await
        .expect("query")
        .expect("still there");
    assert_eq!(
        row.status,
        joyczl_state::TaskStatus::Suspended,
        "an interrupted task stops and waits for a human to resume: {:?}",
        row.last_stop_reason
    );
    assert_eq!(row.rounds_used, 1, "only the interrupted round was spent");
    let runs = server
        .runs
        .list_by_task(&id, 10)
        .await
        .expect("list the runs");
    assert_eq!(runs[0].status, "suspended");
}

/// P1 planner (JOY_PLAN): one extra call before the first round, and its steps land in the
/// ledger as `kind = "plan"` — outside the run, so they can never be mistaken for tool calls or
/// inflate `toolsOk`. Acceptance stays exactly what the human wrote.
#[tokio::test]
async fn the_planner_writes_plan_steps_without_touching_acceptance() {
    let mock = Arc::new(Mock::new(vec![
        // The plan call comes first: the gate, then the model's own round.
        Mock::text("{\"steps\": [\"write the skeleton\", \"then add the verdict\"]}"),
        Mock::text("skip"),
        Mock::tool_use(
            "call_1",
            "write_file",
            json!({ "path": "report.md", "content": "verdict: ready to ship\n" }),
        ),
        Mock::text("written."),
    ]));
    let (server, _home) = boot_with(Some(mock), true).await;
    let dir = tempfile::tempdir().expect("temporary directory");
    let workspace = dir.path().display().to_string();

    let created = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({
            "sessionId": "default",
            "title": "write a release report",
            "cwd": workspace,
            "acceptance": [{ "kind": "fileExists", "path": "report.md" }],
        }),
    )
    .await
    .expect("create the task");
    let id = created["task"]["id"]
        .as_str()
        .expect("it has an id")
        .to_string();

    drive(&server, "default", "let us start", None).await;

    let row = server
        .tasks
        .get(&id)
        .await
        .expect("query")
        .expect("still there");
    assert_eq!(row.status, joyczl_state::TaskStatus::Done);
    // Re-planning is allowed to rewrite the plan; acceptance is not — it is still the human's.
    let acceptance: serde_json::Value =
        serde_json::from_str(&row.acceptance_json).expect("the acceptance conditions");
    assert_eq!(acceptance.as_array().expect("an array").len(), 1);

    let planned = server.steps.list_plan(&id).await.expect("list the plan");
    assert_eq!(planned.len(), 2, "a two-step plan gets two rows");
    assert!(planned.iter().all(|step| step.kind == "plan"));
    assert!(planned.iter().all(|step| step.status == "planned"));
    assert!(
        planned.iter().all(|step| step.run_id.is_none()),
        "the plan does not belong to one run"
    );
    assert!(planned[0]
        .input_json
        .as_deref()
        .is_some_and(|json| json.contains("write the skeleton")));

    // The run's own steps stay tool-only: the plan is not an execution.
    let runs = server
        .runs
        .list_by_task(&id, 10)
        .await
        .expect("list the runs");
    let tool_steps = server
        .steps
        .list_by_run(&runs[0].id)
        .await
        .expect("list the steps");
    assert_eq!(tool_steps.len(), 1);
    assert_eq!(tool_steps[0].kind, "tool");
}

/// A planner that answers nonsense must not take the task down: the round runs without a plan.
#[tokio::test]
async fn a_broken_plan_is_ignored_not_fatal() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text("I think we could do it this way (and I am not giving you JSON)"),
        Mock::text("skip"),
        Mock::tool_use(
            "call_1",
            "write_file",
            json!({ "path": "report.md", "content": "verdict\n" }),
        ),
        Mock::text("written."),
    ]));
    let (server, _home) = boot_with(Some(mock), true).await;
    let dir = tempfile::tempdir().expect("temporary directory");
    let workspace = dir.path().display().to_string();

    let created = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({
            "sessionId": "default",
            "title": "write a report",
            "cwd": workspace,
            "acceptance": [{ "kind": "fileExists", "path": "report.md" }],
        }),
    )
    .await
    .expect("create the task");
    let id = created["task"]["id"]
        .as_str()
        .expect("it has an id")
        .to_string();

    drive(&server, "default", "let us start", None).await;

    let row = server
        .tasks
        .get(&id)
        .await
        .expect("query")
        .expect("still there");
    assert_eq!(
        row.status,
        joyczl_state::TaskStatus::Done,
        "a broken planner does not affect the task itself"
    );
    assert!(server
        .steps
        .list_plan(&id)
        .await
        .expect("list the plan")
        .is_empty());
}
