//! Assembly tests for one turn — this layer answers exactly one question: **how did this turn run**.
//!
//! The model is a scripted fake (`joyczl_provider::mock`); "what the model said on which call" is fully
//! controllable, so every branch of the graph can be pinned. After a run, three things are checked:
//!
//!   1. which notifications the client received — graph node events are visible only at this layer;
//!   2. gate / graph / model in `TurnMeta` — this turn's gate and route;
//!   3. **how many times the model was actually called** — the entire value of quick answers is that number.
//!
//! The criteria live at this layer: the path that exists only when the graph is **on**, and the path
//! without the graph, must differ only by "what is missing", never by "what was answered wrongly".

use std::sync::Arc;

use joyczl_config::Settings;
use joyczl_protocol::{
    ErrorObject, GraphRouteKind, JsonRpcMessage, JsonRpcRequest, RequestId, ServerNotification,
    TokenUsage, TurnMeta, TurnStartParams,
};
use joyczl_provider::mock::Mock;
use joyczl_provider::{CreateRequest, Resolved};
use serde_json::json;
use tokio::sync::mpsc;

use crate::{EventSink, Frame, Server};

/// Everything a test wants to look at after one turn has run.
struct Ran {
    notifications: Vec<ServerNotification>,
    /// Both the quick and full paths emit `TurnCompleted`; only an early failure has none.
    meta: Option<TurnMeta>,
    reply: String,
    /// The requests the model received, in order. This vector is "how much was spent".
    requests: Vec<CreateRequest>,
    error: Option<ErrorObject>,
}

impl Ran {
    fn meta(&self) -> &TurnMeta {
        self.meta
            .as_ref()
            .expect("a finished turn always has TurnCompleted")
    }
}

async fn server(mock: Arc<Mock>, graph_workflows: bool) -> Server {
    let dir = tempfile::tempdir().expect("temporary directory");
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("open the store");
    let settings = Settings {
        home: dir.path().to_path_buf(),
        graph_workflows,
        ..Settings::default()
    };
    // The directory follows the test process — the temp dir is scaffolding here, not the thing under test.
    let _ = dir.keep(); // sqlite still writes -wal/-shm: the directory must not be deleted here

    let server = Server::boot(pool, settings).await;
    // The real provider wants a key; swap in the scripted fake here: another implementation of the same
    // trait — exactly the test capability gained for free by collapsing "model calls" into one trait.
    server.set_resolved(Some(Resolved {
        context_window: joyczl_provider::DEFAULT_CONTEXT_WINDOW,
        vision: false,
        provider_id: "mock".to_string(),
        client: mock,
        model: "test-model".to_string(),
        small_model: "test-small".to_string(),
    }));
    server
}

/// Send one request and await its response (notifications are drained and dropped — for response-focused methods).
async fn call(server: &Server, request: JsonRpcRequest) -> JsonRpcMessage {
    let (tx, mut rx) = mpsc::unbounded_channel();
    crate::handle(server, request, EventSink::new(tx)).await;
    let mut last = None;
    while let Ok(frame) = rx.try_recv() {
        if let Frame::Response(message) = frame {
            last = Some(message);
        }
    }
    last.expect("every request has a reply")
}

fn request(id: i32, method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: RequestId::Number(id),
        method: method.to_string(),
        params: Some(params),
    }
}

async fn run(mock: Mock, message: &str, graph_workflows: bool) -> Ran {
    let mock = Arc::new(mock);
    let server = server(mock.clone(), graph_workflows).await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let sink = EventSink::new(tx);

    let outcome = crate::turn::run_turn(
        &server,
        TurnStartParams {
            attachments: None,
            session_id: Some("test".to_string()),
            message: message.to_string(),
            stream: Some(true),
            cwd: None,
            tier: None,
        },
        RequestId::Number(1),
        &sink,
    )
    .await;

    // Notifications and responses share one channel, with the response last — draining means everything has arrived.
    let mut notifications = Vec::new();
    let mut meta = None;
    let mut reply = String::new();
    while let Ok(frame) = rx.try_recv() {
        match frame {
            Frame::Notification(ServerNotification::TurnCompleted(completed)) => {
                reply = completed.reply.clone();
                meta = Some(completed.meta.clone());
            }
            Frame::Notification(notification) => notifications.push(notification),
            Frame::Response(_) => {}
        }
    }

    // Copy out the received requests before building Ran — the lock guard cannot live to the function's tail expression.
    let requests = mock.received.lock().expect("lock").clone();
    Ran {
        notifications,
        meta,
        reply,
        requests,
        error: outcome.err(),
    }
}

/// The node order as the client saw it.
fn nodes(ran: &Ran) -> Vec<String> {
    ran.notifications
        .iter()
        .filter_map(|notification| match notification {
            ServerNotification::GraphNodeStarted(started) => Some(started.node.clone()),
            _ => None,
        })
        .collect()
}

/// What the retrieval gate said this time — `None` means the turn never reached the gate at all.
fn gate_of(ran: &Ran) -> Option<joyczl_protocol::GateDecisionKind> {
    ran.notifications
        .iter()
        .find_map(|notification| match notification {
            ServerNotification::GateDecided(decided) => Some(decided.decision.decision),
            _ => None,
        })
}

fn has_graph_started(ran: &Ran) -> bool {
    ran.notifications
        .iter()
        .any(|notification| matches!(notification, ServerNotification::GraphStarted(_)))
}

/// Graph off = today's behavior, down to the last word.
#[tokio::test]
async fn graph_off_is_the_plain_path() {
    let mock = Mock::new(vec![
        // Retrieval gate: no need to consult memory.
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "small talk"}"#),
        // THE LOOP.
        Mock::text("Four."),
    ]);
    let ran = run(mock, "What is 2+2?", false).await;

    assert!(ran.error.is_none(), "{:?}", ran.error);
    let meta = ran.meta();
    assert!(
        meta.graph.is_none(),
        "with the graph off there should be no trace of it"
    );
    assert!(
        meta.gate.is_some(),
        "the ordinary path runs a retrieval gate"
    );
    assert_eq!(meta.model, "test-model");
    assert_eq!(ran.reply, "Four.");
    assert!(
        !has_graph_started(&ran),
        "no graph means no graph notification"
    );
    assert_eq!(ran.requests.len(), 2, "one for the gate, one for the loop");
    // Usage is persisted along with meta: token counts no longer live only in turnCompleted.
    assert_eq!(
        meta.usage,
        Some(TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
        })
    );
    assert!(
        !meta.interrupted,
        "nobody interrupted it, and meta has to say so"
    );
}

/// Quick answer: two model calls, one of them the small model's classification. **No retrieval gate**.
#[tokio::test]
async fn quick_route_is_one_short_call_and_no_gate() {
    let mock = Mock::new(vec![
        // Classifier: this line needs no memory lookup; the small model answers directly.
        Mock::text(r#"{"route": "quick", "reason": "greeting"}"#),
        Mock::text("hi there."),
    ]);
    let ran = run(mock, "hello", true).await;

    assert!(ran.error.is_none(), "{:?}", ran.error);
    let meta = ran.meta();
    let graph = meta
        .graph
        .as_ref()
        .expect("taking the graph means there is a graph record");
    assert_eq!(graph.workflow, "triage");
    assert_eq!(graph.route, GraphRouteKind::Quick);
    assert_eq!(graph.reason, "greeting");
    assert!(graph.path.contains(&"quick_reply".to_string()), "{graph:?}");

    assert_eq!(
        gate_of(&ran),
        None,
        "quick personal context is a local read and must not add a retrieval gate"
    );
    assert_eq!(meta.iterations, 1);
    assert_eq!(
        meta.model, "test-small",
        "the quick answer came from the small model, and meta has to say so"
    );
    assert_eq!(ran.reply, "hi there.");

    // Two calls: one classify, one quick answer. No third — that is the entire cheapness of the quick path.
    assert_eq!(ran.requests.len(), 2);
    assert!(ran.requests[0].messages[0].text().contains("triage gate"));

    // The graph notifications went out, and the node order matches what meta recorded.
    assert!(has_graph_started(&ran));
    let walked = nodes(&ran);
    assert!(walked.contains(&"classify".to_string()), "{walked:?}");
    assert!(walked.contains(&"quick_reply".to_string()), "{walked:?}");
    assert_eq!(
        ran.notifications
            .iter()
            .filter(|n| matches!(n, ServerNotification::GraphEnded(_)))
            .count(),
        1,
        "the graph has a beginning and an end"
    );
}

/// Full path: the gate is present and the big model still answers — the **same turn** as without the graph.
#[tokio::test]
async fn full_route_is_the_same_turn_as_the_plain_path() {
    let mock = Mock::new(vec![
        Mock::text(r#"{"route": "full", "reason": "about a person"}"#),
        Mock::text(r#"{"retrieve": true, "query": "Alex meeting", "reason": "about a person"}"#),
        Mock::text("You are meeting Alex on Thursday at 3pm."),
    ]);
    let ran = run(mock, "when am I meeting Alex?", true).await;

    assert!(ran.error.is_none(), "{:?}", ran.error);
    let meta = ran.meta();
    let graph = meta
        .graph
        .as_ref()
        .expect("taking the graph means a graph record");
    assert_eq!(graph.route, GraphRouteKind::Full);
    assert!(graph.path.contains(&"full_agent".to_string()), "{graph:?}");

    assert_eq!(
        gate_of(&ran),
        Some(joyczl_protocol::GateDecisionKind::Retrieve)
    );
    assert_eq!(
        meta.model, "test-model",
        "the full path still uses the big model"
    );
    assert_eq!(ran.reply, "You are meeting Alex on Thursday at 3pm.");
    // Classify + gate + loop.
    assert_eq!(ran.requests.len(), 3);
    assert!(
        ran.requests[1].messages[0]
            .text()
            .contains("retrieval gate"),
        "the second call is the gate, nothing else"
    );
}

/// A loop failure inside the graph: reported once, honestly, **no retry** — a retry would just fail the same way again.
#[tokio::test]
async fn a_failed_loop_inside_the_graph_is_reported_without_a_retry() {
    // The classifier said full, then nothing: the gate and the loop each hit the empty queue once.
    let mock = Mock::new(vec![Mock::text(r#"{"route": "full", "reason": "x"}"#)]);
    let ran = run(mock, "take a look for me", true).await;

    let error = ran.error.expect("this should have failed");
    assert_eq!(error.code, joyczl_protocol::codes::PROVIDER_ERROR);
    assert!(error.message.contains("Model call failed"), "{error:?}");

    // Three attempts: classify, gate, loop. One more would mean a retry happened.
    assert_eq!(ran.requests.len(), 3);
    // The graph itself finishes normally (node errors are recorded, not thrown) — what failed was the loop inside.
    assert_eq!(
        ran.notifications
            .iter()
            .filter(|n| matches!(n, ServerNotification::GraphEnded(_)))
            .count(),
        1
    );
}

/// A misconfigured MCP server must not keep the whole assistant from starting.
#[tokio::test]
async fn a_broken_mcp_server_still_leaves_the_builtin_tools() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("open the store");
    std::fs::write(
        dir.path().join("mcp.json"),
        json!({"servers": [{"name": "ghost", "command": "/definitely/not/a/binary"}]}).to_string(),
    )
    .expect("write the config");

    let settings = Settings {
        home: dir.path().to_path_buf(),
        ..Settings::default()
    };
    let server = Server::boot(pool, settings).await;

    let names = server.tools.names();
    assert!(names.contains(&"save_note"), "{names:?}");
    assert!(
        !names.iter().any(|name| name.starts_with("ghost")),
        "an unreachable server must not leave half a tool behind: {names:?}"
    );
}

/// Tools configured in `<home>/mcp.json` really land in the server's tool table — end to end.
///
/// The fake server is a real subprocess (`python3`, no SDK), consistent with the MCP side's approach:
/// what this layer verifies is that the "config → subprocess → tool table" line works end to end.
#[tokio::test]
async fn an_mcp_server_next_to_home_lands_in_the_tool_table() {
    let Some(python) = python3() else {
        eprintln!("skipped: no python3 on this machine");
        return;
    };
    let dir = tempfile::tempdir().expect("temporary directory");
    let script = dir.path().join("fake_mcp_server.py");
    std::fs::write(&script, FAKE_SERVER).expect("write the server");
    std::fs::write(
        dir.path().join("mcp.json"),
        json!({"servers": [{"name": "demo", "command": python, "args": [script]}]}).to_string(),
    )
    .expect("write the config");

    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("open the store");
    let settings = Settings {
        home: dir.path().to_path_buf(),
        ..Settings::default()
    };
    let server = Server::boot(pool, settings).await;

    let names = server.tools.names();
    assert!(
        names.contains(&"demo_echo"),
        "MCP tools enter the table with their server name as prefix: {names:?}"
    );
    assert!(
        names.contains(&"current_time"),
        "the server's own tool is still there: {names:?}"
    );
}

fn python3() -> Option<String> {
    let out = std::process::Command::new("python3")
        .arg("--version")
        .output()
        .ok()?;
    out.status.success().then(|| "python3".to_string())
}

/// An MCP server that only handles initialize + tools/list.
const FAKE_SERVER: &str = r#"
import json, sys

def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    if "id" not in msg:
        continue
    if msg.get("method") == "initialize":
        result = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                  "serverInfo": {"name": "fake", "version": "1"}}
    elif msg.get("method") == "tools/list":
        result = {"tools": [{"name": "echo",
                             "inputSchema": {"type": "object", "properties": {}}}]}
    else:
        send({"jsonrpc": "2.0", "id": msg["id"],
              "error": {"code": -32601, "message": "no such method"}})
        continue
    send({"jsonrpc": "2.0", "id": msg["id"], "result": result})
"#;

/// The tool-start notification must arrive **before** the tool-completed notification — only then can
/// the client draw "calling X". That is the entire reason ToolStarted exists, apart from turn/interrupt.
#[tokio::test]
async fn tool_started_goes_out_before_tool_completed() {
    let mock = Mock::new(vec![
        // Retrieval gate.
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "small talk"}"#),
        Mock::tool_use("tu_1", "save_note", json!({"subject": "a", "content": "b"})),
        Mock::text("noted."),
    ]);
    let ran = run(mock, "remind me of something", false).await;

    assert!(ran.error.is_none(), "{:?}", ran.error);
    let order: Vec<&str> = ran
        .notifications
        .iter()
        .filter_map(|n| match n {
            ServerNotification::ToolStarted(_) => Some("started"),
            ServerNotification::ToolCompleted(_) => Some("completed"),
            _ => None,
        })
        .collect();
    assert_eq!(
        order,
        vec!["started", "completed"],
        "ToolStarted has to come before ToolCompleted, each exactly once"
    );
}

/// The per-session write-section lock is **lazy but stable**: the same session always gets the
/// same lock (two concurrent turns must not end up holding two different ones), and different
/// sessions never share one — that would serialise the whole server for no reason.
#[tokio::test]
async fn the_write_lock_is_lazy_but_stable_per_session() {
    let server = server(Arc::new(Mock::new(vec![])), false).await;

    let first = server.session_write_lock("s1");
    let again = server.session_write_lock("s1");
    let other = server.session_write_lock("s2");

    assert!(Arc::ptr_eq(&first, &again), "one lock per session");
    assert!(
        !Arc::ptr_eq(&first, &other),
        "different sessions get different locks"
    );
}

/// The lock run_turn takes is a real mutex: while one task holds it, another's acquire
/// does not go through — that is the whole guarantee the write sections (append /
/// consolidate / compact) rely on to stop interleaving.
#[tokio::test]
async fn a_held_write_lock_blocks_another_acquirer() {
    let server = server(Arc::new(Mock::new(vec![])), false).await;
    let lock = server.session_write_lock("s");

    let guard = lock.lock().await;
    let attempt = lock.clone();
    let blocked = tokio::time::timeout(std::time::Duration::from_millis(50), async move {
        let _ = attempt.lock().await;
    })
    .await;
    assert!(
        blocked.is_err(),
        "while the write lock is held another taker has to wait"
    );

    drop(guard);
    let relock = lock.clone();
    let acquired = tokio::time::timeout(std::time::Duration::from_millis(50), async move {
        let guard = relock.lock().await;
        drop(guard);
    })
    .await;
    assert!(
        acquired.is_ok(),
        "after the release the next taker gets in at once"
    );
}

/// turn/interrupt: a running turn's token can be flipped; a non-running turn cannot be interrupted —
/// but it must say false honestly, instead of pretending something was interrupted.
#[tokio::test]
async fn interrupt_hits_a_running_turn_and_misses_a_finished_one() {
    let mock = Arc::new(Mock::new(vec![]));
    let server = server(mock, false).await;

    // A turn that was never registered.
    let response = call(
        &server,
        request(1, "turn/interrupt", json!({"turnId": "t_missing"})),
    )
    .await;
    match response {
        JsonRpcMessage::Response(resp) => {
            assert_eq!(resp.result, json!({"interrupted": false}))
        }
        other => panic!("interrupting a turn that does not exist should not error: {other:?}"),
    }

    // A running turn: the response says it hit, and the token really flipped.
    let (interrupt, _) = server
        .begin_turn("t_live", "default")
        .expect("the session is idle");
    let response = call(
        &server,
        request(2, "turn/interrupt", json!({"turnId": "t_live"})),
    )
    .await;
    match response {
        JsonRpcMessage::Response(resp) => {
            assert_eq!(resp.result, json!({"interrupted": true}))
        }
        other => panic!("interrupting a running turn should not error: {other:?}"),
    }
    assert!(
        interrupt.is_cancelled(),
        "the token has to be cancelled for real"
    );
    server.finish_turn("t_live");
}

/// config/write: takes effect in memory, lands on disk, survives a reboot — all three or nothing.
#[tokio::test]
async fn config_write_lands_in_memory_and_survives_a_reboot() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("open the store");
    let _ = dir.keep(); // sqlite still writes -wal/-shm: the directory must not be deleted here
    let settings = Settings {
        home: home.clone(),
        ..Settings::default()
    };
    let server = Server::boot(pool, settings).await;

    let response = call(
        &server,
        request(
            1,
            "config/write",
            // Runtime parameters go through config/write; provider and models are not on this path (see the next test).
            json!({"patch": {"maxIterations": 20, "maxTokens": 4096}}),
        ),
    )
    .await;
    match response {
        JsonRpcMessage::Response(resp) => {
            assert_eq!(resp.result["config"]["maxIterations"], 20);
            assert_eq!(resp.result["config"]["maxTokens"], 4096);
        }
        other => panic!("config/write should not error: {other:?}"),
    }
    // The next turn in memory uses the new values too.
    assert_eq!(server.settings().max_iterations, 20);
    assert_eq!(server.settings().max_tokens, 4096);
    // There is a record on disk.
    assert!(
        home.join("settings.json").exists(),
        "the patch has to reach the disk"
    );

    // Reboot: open the same home again, patches stacked on top of env values.
    let settings2 = Settings {
        home,
        ..Settings::default()
    };
    let server2 = crate::open(&settings2).await.expect("reopen");
    assert_eq!(server2.settings().max_iterations, 20);
    assert_eq!(server2.settings().max_tokens, 4096);
}

/// Models and keys are **not** on the config/write path: the patch has no provider / model / key fields;
/// changing them only goes through `model/*`. This assertion pins "the old road has been torn out".
#[tokio::test]
async fn config_write_cannot_smuggle_a_model_or_a_key_in() {
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
            ..Settings::default()
        },
    )
    .await;

    let response = call(
        &server,
        request(
            1,
            "config/write",
            json!({"patch": {"provider": "deepseek", "model": "deepseek-flash", "apiKey": "sk-leaked-in"}}),
        ),
    )
    .await;
    match response {
        JsonRpcMessage::Response(resp) => {
            // Unknown fields are ignored (not an error) — the point is that they **do not take effect**.
            assert_eq!(resp.result["config"]["provider"], "anthropic");
        }
        other => panic!("should not error: {other:?}"),
    }
    assert_eq!(server.settings().provider, "anthropic");
    assert_eq!(server.settings().model, None);
    assert_eq!(server.settings().api_key, None);
    let saved = std::fs::read_to_string(home.join("settings.json")).unwrap_or_default();
    assert!(
        !saved.contains("sk-"),
        "a key must never reach settings.json: {saved}"
    );
}

/// config/write validation: a bad patch is rejected wholesale — half a patch is worse than none.
#[tokio::test]
async fn a_bad_patch_is_rejected_without_touching_anything() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("open the store");
    let _ = dir.keep(); // sqlite still writes -wal/-shm: the directory must not be deleted here
    let settings = Settings {
        home: home.clone(),
        ..Settings::default()
    };
    let server = Server::boot(pool, settings).await;

    let response = call(
        &server,
        request(1, "config/write", json!({"patch": {"maxIterations": 0}})),
    )
    .await;
    match response {
        JsonRpcMessage::Error(error) => {
            assert_eq!(error.error.code, joyczl_protocol::codes::INVALID_PARAMS);
            assert!(error.error.message.contains("maxIterations"));
        }
        other => panic!("an illegal patch must be refused: {other:?}"),
    }
    // Nothing changed: memory still holds the defaults, and no patch file is on disk.
    assert_eq!(
        server.settings().max_iterations,
        joyczl_config::Settings::default().max_iterations
    );
    assert!(!home.join("settings.json").exists());
}

/// model/list: the catalog is the PROVIDERS table, narrowed by the provider filter.
#[tokio::test]
async fn model_list_reports_the_provider_catalog() {
    let mock = Arc::new(Mock::new(vec![]));
    let server = server(mock, false).await;

    let response = call(&server, request(1, "model/list", json!({}))).await;
    match response {
        JsonRpcMessage::Response(resp) => {
            let data = resp.result["data"].as_array().expect("the data array");
            assert_eq!(data.len(), joyczl_provider::PROVIDERS.len());
            assert_eq!(data[0]["provider"], "anthropic");
            assert_eq!(data[0]["flagship"], "claude-sonnet-5");
        }
        other => panic!("model/list should not error: {other:?}"),
    }

    let response = call(
        &server,
        request(2, "model/list", json!({"provider": "deepseek"})),
    )
    .await;
    match response {
        JsonRpcMessage::Response(resp) => {
            let data = resp.result["data"].as_array().expect("the data array");
            assert_eq!(data.len(), 1);
            assert_eq!(data[0]["provider"], "deepseek");
        }
        other => panic!("model/list with a filter should not error: {other:?}"),
    }
}

/// After one turn: trace landed in traces/<date>.jsonl, usage landed in usage.jsonl —
/// observability is not optional, but a write failure must not drag the conversation down either.
#[tokio::test]
async fn a_turn_leaves_a_trace_and_a_usage_line_behind() {
    let mock = Mock::new(vec![
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "small talk"}"#),
        Mock::text("Four."),
    ]);
    let server = server(Arc::new(mock), false).await;
    let home = server.settings().home.clone();

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    crate::run_turn(
        &server,
        TurnStartParams {
            attachments: None,
            session_id: Some("test".to_string()),
            message: "What is 2+2?".to_string(),
            stream: Some(true),
            cwd: None,
            tier: None,
        },
        RequestId::Number(1),
        &EventSink::new(tx),
    )
    .await
    .expect("this turn should get through");

    // The trace file is named by date, one line per turn, each line carrying turnId / meta.
    let mut traces = std::fs::read_dir(home.join("traces"))
        .expect("a traces directory")
        .collect::<Vec<_>>();
    assert_eq!(traces.len(), 1, "one file per turn: {:?}", traces);
    let trace = std::fs::read_to_string(traces.remove(0).unwrap().path()).unwrap();
    let record: serde_json::Value = serde_json::from_str(trace.trim()).expect("valid JSON");
    assert_eq!(record["userMessage"], "What is 2+2?");
    assert_eq!(record["meta"]["model"], "test-model");
    assert_eq!(
        record["meta"]["usage"]["outputTokens"], 5,
        "usage lands on disk with meta"
    );

    // The usage ledger: one line per turn.
    let usage = std::fs::read_to_string(home.join("usage.jsonl")).unwrap();
    let row: serde_json::Value = serde_json::from_str(usage.trim()).expect("valid JSON");
    assert_eq!(row["provider"], "mock");
    assert_eq!(row["inputTokens"], 10);
    assert_eq!(row["iterations"], 1);
}

// ---- Context overflow: retry once after compaction ---------------------------------------------

/// Overflow is the only error worth retrying: compact once, retry, and on success wind down as usual.
#[tokio::test]
async fn a_context_overflow_is_compacted_and_retried_exactly_once() {
    let mock = Arc::new(Mock::with_outcomes(vec![
        // Gate
        Ok(Mock::text(
            r#"{"retrieve": false, "query": "", "reason": "small talk"}"#,
        )),
        // First loop: the provider says the context overflowed
        Mock::context_overflow(),
        // Forced compaction before the retry (that one summary model call)
        Ok(Mock::text("a summary of the earlier conversation.")),
        // Second loop: success
        Ok(Mock::text("Four.")),
    ]));
    let server = server(mock.clone(), false).await;
    // Accumulate a few turns of history — forced compaction needs something to fold.
    for i in 0..4 {
        server
            .chat
            .append_exchange(
                &format!("question {i}"),
                &format!("answer {i}"),
                "test",
                "cli",
                None,
            )
            .await
            .expect("write the session");
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    crate::turn::run_turn(
        &server,
        TurnStartParams {
            attachments: None,
            session_id: Some("test".to_string()),
            message: "What is 2+2?".to_string(),
            stream: Some(true),
            cwd: None,
            tier: None,
        },
        RequestId::Number(1),
        &EventSink::new(tx),
    )
    .await
    .expect("the retry after the overflow should get through");

    let mut reply = String::new();
    while let Ok(frame) = rx.try_recv() {
        if let Frame::Notification(ServerNotification::TurnCompleted(done)) = frame {
            reply = done.reply.clone();
        }
    }
    assert_eq!(reply, "Four.");
    assert_eq!(
        mock.received.lock().expect("lock").len(),
        4,
        "gate + overflow + summary + retry, not one call extra"
    );
    assert!(
        server
            .chat
            .load_rollup("test")
            .await
            .expect("query the store")
            .is_some(),
        "a forced compaction has to leave a summary"
    );
}

/// Other errors are **not** retried: a retry would just fail the same way again (wasting double the time).
#[tokio::test]
async fn a_plain_provider_error_is_not_retried() {
    let mock = Arc::new(Mock::with_outcomes(vec![
        Ok(Mock::text(
            r#"{"retrieve": false, "query": "", "reason": "small talk"}"#,
        )),
        Err(joyczl_provider::ProviderError::Api("boom".to_string())),
    ]));
    let server = server(mock.clone(), false).await;
    let (tx, _rx) = mpsc::unbounded_channel();

    let error = crate::turn::run_turn(
        &server,
        TurnStartParams {
            attachments: None,
            session_id: Some("test".to_string()),
            message: "hello".to_string(),
            stream: Some(true),
            cwd: None,
            tier: None,
        },
        RequestId::Number(1),
        &EventSink::new(tx),
    )
    .await
    .expect_err("it has to report honestly");

    assert_eq!(error.code, joyczl_protocol::codes::PROVIDER_ERROR);
    assert_eq!(
        mock.received.lock().expect("lock").len(),
        2,
        "one for the gate, one for the loop, no third model call"
    );
}

// ---- Rate-limit retry: real HTTP, real client --------------------------------------------

/// Read a request **completely** (headers + the body named by `Content-Length`) before answering.
///
/// Answering before the read finishes and then closing the connection knocks over a client still writing
/// its body — mostly harmless on macOS/Linux, but on Windows it shows up as an RST, and the client sees
/// "error sending request", so "the server clearly replied" becomes a network error. This test's request
/// carries the full system prompt and tool table, so the body exceeds one read's worth —
/// which is why it reproduces stably on Windows only.
async fn drain_request(socket: &mut tokio::net::TcpStream) {
    // The imports live inside the function: these test files each use what they need elsewhere; this trait is not referenced elsewhere.
    use tokio::io::AsyncReadExt;

    let mut seen: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => {
                seen.extend_from_slice(&chunk[..read]);
                let Some(head_end) = seen.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&seen[..head_end]).to_lowercase();
                let want: usize = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse().ok())
                    .unwrap_or(0);
                if seen.len() >= head_end + 4 + want {
                    return;
                }
            }
        }
    }
}

/// A minimal HTTP server: answers one by one from the script. Returns the base_url.
///
/// No test framework and no Mock here — what is being pinned is exactly "a real OpenAI client backs off
/// and retries on a real 429"; a fake client cannot test that.
async fn scripted_http(responses: Vec<(u16, String)>) -> String {
    use tokio::io::AsyncWriteExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the port");
    let addr = listener.local_addr().expect("the local address");
    tokio::spawn(async move {
        let mut index = 0usize;
        while let Ok((mut socket, _)) = listener.accept().await {
            drain_request(&mut socket).await;
            let (status, body) = responses
                .get(index)
                .cloned()
                .unwrap_or((200, "{}".to_string()));
            index += 1;
            let reason = if status == 429 {
                "Too Many Requests"
            } else {
                "OK"
            };
            // `Retry-After: 0`: verifies it is read, while keeping the test from truly waiting.
            let extra = if status == 429 {
                "Retry-After: 0\r\n"
            } else {
                ""
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });
    format!("http://{addr}")
}

/// One complete (non-streaming) response — the gate takes this path.
fn completion(text: &str) -> String {
    serde_json::json!({"status":"completed", "output":[{"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":text}]}], "usage":{"input_tokens":1,"output_tokens":1}}).to_string()
}

fn sse_text(text: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\n",
        serde_json::json!({"type":"response.output_text.delta", "delta":text}),
        serde_json::json!({"type":"response.completed", "response":serde_json::from_str::<serde_json::Value>(&completion(text)).unwrap()})
    )
}

/// A 429 is retried, and the retry is **visible**: one Retry notification + `meta.retries`.
#[tokio::test]
async fn a_rate_limited_call_is_retried_and_reported_in_the_meta() {
    let base = scripted_http(vec![
        (429, r#"{"error":{"message":"slow down"}}"#.to_string()),
        // Gate (its turn comes only after the retry)
        (
            200,
            completion(r#"{"retrieve": false, "query": "", "reason": "small talk"}"#),
        ),
        // Loop
        (200, sse_text("Four.")),
    ])
    .await;

    let dir = tempfile::tempdir().expect("temporary directory");
    let settings = joyczl_config::Settings {
        home: dir.path().to_path_buf(),
        provider: "openai".to_string(),
        api_key: Some("test-key".to_string()),
        base_url: Some(base),
        model: Some("test-model".to_string()),
        small_model: Some("test-model".to_string()),
        max_tokens: 256,
        ..Default::default()
    };
    let server = crate::open(&settings)
        .await
        .expect("assemble with the real client");

    let (tx, mut rx) = mpsc::unbounded_channel();
    crate::turn::run_turn(
        &server,
        TurnStartParams {
            attachments: None,
            session_id: Some("test".to_string()),
            message: "What is 2+2?".to_string(),
            stream: Some(true),
            cwd: None,
            tier: None,
        },
        RequestId::Number(1),
        &EventSink::new(tx),
    )
    .await
    .expect("after the rate limit it should finish as usual");

    let mut retries = 0;
    let mut meta_retries = -1;
    while let Ok(frame) = rx.try_recv() {
        if let Frame::Notification(notification) = frame {
            match notification {
                ServerNotification::Retry(retry) => {
                    retries += 1;
                    assert!(retry.reason.contains("429"), "{retry:?}");
                    assert!(
                        !retry.turn_id.is_empty(),
                        "the notification has to name the turn"
                    );
                }
                ServerNotification::TurnCompleted(done) => {
                    meta_retries = done.meta.retries;
                }
                _ => {}
            }
        }
    }
    assert_eq!(retries, 1, "every retry owes a notification");
    assert_eq!(
        meta_retries, 1,
        "the count in meta is measured, not guessed"
    );
}

/// Working-directory admission (fail-closed): valid ones pass; relative and nonexistent paths are blocked;
/// an empty string counts as not given — the same semantics as the protocol side's "default = process cwd".
#[test]
fn turn_cwd_validation_is_fail_closed() {
    use crate::turn::resolve_turn_cwd;

    let dir = tempfile::tempdir().expect("temporary directory");
    let ok = resolve_turn_cwd(&Some(dir.path().display().to_string()));
    assert!(
        ok.unwrap().is_some(),
        "an absolute path that exists should pass"
    );

    assert!(resolve_turn_cwd(&None).unwrap().is_none());
    assert!(resolve_turn_cwd(&Some(String::new())).unwrap().is_none());
    assert!(resolve_turn_cwd(&Some("   ".into())).unwrap().is_none());

    // Relative path: rejected, and the error speaks plainly.
    let err = resolve_turn_cwd(&Some("relative/path".into())).unwrap_err();
    assert!(err.message.contains("absolute path"), "{}", err.message);

    // A nonexistent absolute path: rejected.
    let err = resolve_turn_cwd(&Some("/no/such/directory/joy".into())).unwrap_err();
    assert!(err.message.contains("does not exist"), "{}", err.message);
}

// ---- Attachments (multimodal): an image becomes a block before the provider sees it,
// ---- and the vision gate refuses it first ----

/// The same flow as `run`, plus a vision switch, a list of attachments and a "set up before\n/// starting" callback that writes the uploaded files into home.
async fn run_with_attachments(
    mock: Mock,
    vision: bool,
    message: &str,
    attachments: Option<Vec<joyczl_protocol::TurnAttachment>>,
    prepare: impl FnOnce(&std::path::Path),
) -> Ran {
    let mock = Arc::new(mock);
    let dir = tempfile::tempdir().expect("temporary directory");
    prepare(dir.path());
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("open the store");
    let settings = Settings {
        home: dir.path().to_path_buf(),
        ..Settings::default()
    };
    let _ = dir.keep();
    let server = Server::boot(pool, settings).await;
    server.set_resolved(Some(Resolved {
        context_window: joyczl_provider::DEFAULT_CONTEXT_WINDOW,
        vision,
        provider_id: "mock".to_string(),
        client: mock.clone(),
        model: "test-model".to_string(),
        small_model: "test-small".to_string(),
    }));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let sink = EventSink::new(tx);
    let outcome = crate::turn::run_turn(
        &server,
        TurnStartParams {
            attachments,
            session_id: Some("test".to_string()),
            message: message.to_string(),
            stream: Some(true),
            cwd: None,
            tier: None,
        },
        RequestId::Number(1),
        &sink,
    )
    .await;
    let mut notifications = Vec::new();
    let mut meta = None;
    let mut reply = String::new();
    while let Ok(frame) = rx.try_recv() {
        match frame {
            Frame::Response(_) => {}
            Frame::Notification(ServerNotification::TurnCompleted(done)) => {
                reply = done.reply.clone();
                meta = Some(done.meta.clone());
            }
            Frame::Notification(notification) => notifications.push(notification),
        }
    }
    let requests = mock.received.lock().expect("lock").clone();
    Ran {
        notifications,
        meta,
        reply,
        requests,
        error: outcome.err(),
    }
}

fn png_attachment(path: &str) -> Option<Vec<joyczl_protocol::TurnAttachment>> {
    Some(vec![joyczl_protocol::TurnAttachment {
        path: path.to_string(),
        media_type: None,
    }])
}

#[tokio::test]
async fn an_attachment_reaches_the_provider_as_an_image_block() {
    // A real PNG header (1×1): it only passes if the dimensions can be read locally — a
    // 12-byte fake signature counts as unreadable.
    let png = {
        let mut bytes = vec![0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&13u32.to_be_bytes()); // IHDR data length
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    };
    let ran = run_with_attachments(
        Mock::new(vec![
            // The retrieval gate burns one call first (the small model).
            Mock::text(r#"{"retrieve": false, "query": "", "reason": "small talk"}"#),
            Mock::text("the image is a colour patch."),
        ]),
        true,
        "what is in this image?",
        png_attachment("uploads/px.png"),
        |home| {
            let dir = home.join("uploads");
            std::fs::create_dir_all(&dir).expect("create the upload directory");
            std::fs::write(dir.join("px.png"), png.clone()).expect("place the image");
        },
    )
    .await;
    assert!(ran.error.is_none(), "{:?}", ran.error);
    // The gate call comes first and the main call second: scan every request for the user
    // message that carries the image.
    let image = ran
        .requests
        .iter()
        .flat_map(|r| r.messages.iter())
        .filter(|m| matches!(m.role, joyczl_provider::Role::User))
        .flat_map(|m| m.content.iter())
        .find_map(|b| match b {
            joyczl_provider::ContentBlock::Image { media_type, data } => {
                Some((media_type.as_str(), data.as_str()))
            }
            _ => None,
        })
        .expect("the user message carries an Image block");
    assert_eq!(
        image.0, "image/png",
        "the type comes from the extension sniff"
    );
    use base64::Engine as _;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(image.1)
            .expect("valid base64"),
        png,
        "the bytes reach the provider unchanged"
    );
    // The body text is still there (the chat history and the model both see what was said).
    let with_text = ran
        .requests
        .iter()
        .flat_map(|r| r.messages.iter())
        .filter(|m| matches!(m.role, joyczl_provider::Role::User))
        .any(|m| m.text().contains("what is in this image?"));
    assert!(
        with_text,
        "the user's text rides into the request with the image"
    );
}

/// A tall screenshot (1120×11100) is barely over 2 MB of bytes: the old "20 MiB" gate let it
/// through and the far end refused it. Now it has to be stopped before the request goes out,
/// with the real dimensions in the error (the far end only says "unsupported image").
#[tokio::test]
async fn an_oversized_image_is_refused_with_its_real_dimensions() {
    let ran = run_with_attachments(
        Mock::new(vec![]),
        true,
        "have a look at this long image",
        png_attachment("uploads/long.png"),
        |home| {
            let mut bytes = vec![0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
            bytes.extend_from_slice(&13u32.to_be_bytes());
            bytes.extend_from_slice(b"IHDR");
            bytes.extend_from_slice(&1120u32.to_be_bytes());
            bytes.extend_from_slice(&11_100u32.to_be_bytes());
            bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
            let dir = home.join("uploads");
            std::fs::create_dir_all(&dir).expect("create the upload directory");
            std::fs::write(dir.join("long.png"), bytes).expect("place the image");
        },
    )
    .await;
    let error = ran
        .error
        .as_ref()
        .expect("over the per-side limit it has to stop at the door");
    assert!(
        error.message.contains("1120×11100"),
        "the error has to give the real size: {}",
        error.message
    );
    assert!(
        error.message.contains("in pieces"),
        "the error has to give an actionable next step: {}",
        error.message
    );
    assert!(
        ran.requests.is_empty(),
        "a failure must not burn a model call"
    );
}

#[tokio::test]
async fn attachments_fail_up_front_on_a_non_vision_provider() {
    let ran = run_with_attachments(
        Mock::new(vec![]),
        false,
        "have a look at this image",
        png_attachment("uploads/px.png"),
        |_| {},
    )
    .await;
    let error = ran
        .error
        .as_ref()
        .expect("with vision off it has to stop at the door");
    assert!(
        error.message.contains("cannot take images"),
        "{}",
        error.message
    );
    assert!(
        ran.requests.is_empty(),
        "a failure must not burn a model call"
    );
}

#[tokio::test]
async fn an_attachment_outside_home_is_refused_before_the_turn() {
    // A platform-neutral escape vector: `..` out of home. The file has to really exist — only
    // then does canonicalize tell "missing" apart from "outside", and only then does the
    // assertion pin the escape error rather than the missing one.
    let ran = run_with_attachments(
        Mock::new(vec![]),
        true,
        "have a look at this image",
        png_attachment("../escape.png"),
        |home| {
            std::fs::write(
                home.parent()
                    .expect("a tempdir always has a parent")
                    .join("escape.png"),
                [0u8; 8],
            )
            .expect("place the escaping file");
        },
    )
    .await;
    let error = ran
        .error
        .as_ref()
        .expect("a path outside the workspace must be refused");
    assert!(error.message.contains("outside home"), "{}", error.message);
    assert!(ran.requests.is_empty());
}

/// A Unix-only escape vector: a symlink pointing outside home, where the path itself exists.
/// Symlinks need privileged semantics on Windows, so the `../` case above covers that gate.
#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_attachment_outside_home_is_refused() {
    let ran = run_with_attachments(
        Mock::new(vec![]),
        true,
        "have a look at this image",
        png_attachment("uploads/escape.png"),
        |home| {
            let dir = home.join("uploads");
            std::fs::create_dir_all(&dir).expect("create the upload directory");
            std::os::unix::fs::symlink("/etc/hosts", dir.join("escape.png"))
                .expect("create the escaping symlink");
        },
    )
    .await;
    let error = ran
        .error
        .as_ref()
        .expect("a path outside the workspace must be refused");
    assert!(error.message.contains("outside home"), "{}", error.message);
    assert!(ran.requests.is_empty());
}

/// 0.15: with no cwd on `turn/start`, the directory the **session remembers** is used — the
/// whole of "the working directory follows the session" lives in this one branch, which is
/// why it deserves a test of its own.
#[tokio::test]
async fn the_session_working_directory_is_used_when_the_turn_names_none() {
    let server = server(Arc::new(Mock::new(vec![])), false).await;
    // "/tmp" is Unix-only: on Windows it is not a directory, so session_cwd() correctly
    // drops it and the assertion below fails. Use a directory that exists everywhere.
    let cwd = std::env::temp_dir();
    let cwd_text = cwd.to_string_lossy().into_owned();
    server
        .chat
        .create_session("s1", None, Some(cwd_text.as_str()))
        .await
        .expect("create the session");

    assert_eq!(
        crate::turn::session_cwd(&server, "s1").await.as_deref(),
        Some(cwd.as_path()),
        "the directory the session remembers is this turn's writable root"
    );
    assert_eq!(
        crate::turn::session_cwd(&server, "s2").await,
        None,
        "with no such session, no directory may be invented"
    );
    assert_eq!(
        crate::turn::session_cwd(&server, "default").await,
        None,
        "a session that never chose a directory has none (writes are refused, not dropped into the process cwd)"
    );
}

/// The directory was deleted after it was picked: falling back to the process cwd would put
/// writes somewhere nobody expects.
#[tokio::test]
async fn a_vanished_session_directory_is_not_silently_replaced() {
    let server = server(Arc::new(Mock::new(vec![])), false).await;
    let work = tempfile::tempdir().expect("temporary directory");
    let path = work.path().to_string_lossy().to_string();
    server
        .chat
        .create_session("s1", None, Some(&path))
        .await
        .expect("create the session");
    assert!(crate::turn::session_cwd(&server, "s1").await.is_some());

    drop(work);
    assert_eq!(
        crate::turn::session_cwd(&server, "s1").await,
        None,
        "the directory is gone, so there is no writable scope — another directory must not stand in"
    );
}

/// `turn/list` reports only what is still running: a finished turn must disappear, and
/// another session's turns must not be counted in.
#[tokio::test]
async fn turn_list_reports_only_what_is_still_running() {
    let server = server(Arc::new(Mock::new(vec![])), false).await;
    let _token = server.begin_turn("t1", "s1").expect("the session is idle");

    let all = server.active_turns(None);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].0, "t1");
    assert_eq!(all[0].1, "s1");
    assert!(
        server.active_turns(Some("s2")).is_empty(),
        "another session does not leak in"
    );
    assert_eq!(server.active_turns(Some("s1")).len(), 1);

    server.finish_turn("t1");
    assert!(
        server.active_turns(None).is_empty(),
        "once it is done it must be gone"
    );
}

/// The first half of 0.16: **one session runs one turn at a time**.
///
/// This invariant did not exist before: two `turn/start` calls ran in parallel, writes
/// interleaved, and each one saw only half the history.
#[tokio::test]
async fn one_turn_at_a_time_per_session() {
    let server = server(Arc::new(Mock::new(vec![])), false).await;

    let (_interrupt, _steering) = server
        .begin_turn("t1", "s1")
        .expect("the first one should get the session");
    let holder = server
        .begin_turn("t2", "s1")
        .expect_err("a second turn on the same session must be refused");
    assert_eq!(
        holder, "t1",
        "the refusal has to carry the occupant's turn id"
    );

    assert!(
        server.begin_turn("t3", "s2").is_ok(),
        "another session must not be dragged in"
    );

    server.finish_turn("t1");
    assert!(
        server.begin_turn("t4", "s1").is_ok(),
        "once the previous turn ends, it should pass"
    );
}

/// The second half of 0.16: a follow-up goes into **the turn that is running**; with no turn
/// running it says so, rather than silently becoming a new one.
#[tokio::test]
async fn steering_needs_a_running_turn_and_keeps_arrival_order() {
    let server = server(Arc::new(Mock::new(vec![])), false).await;

    assert!(
        server.steer("s1", "just one more thing").is_err(),
        "with no turn running it has to say so — silently starting a new turn would be a lie"
    );

    let (_interrupt, steering) = server.begin_turn("t1", "s1").expect("the session is idle");
    let (turn_id, queued) = server
        .steer("s1", "have a look at the README too")
        .expect("a turn is running");
    assert_eq!(turn_id, "t1");
    assert_eq!(queued, 1);
    let (_, queued) = server
        .steer("s1", "a second look at the CHANGELOG")
        .expect("a turn is running");
    assert_eq!(queued, 2);

    let mut channel = steering.lock().expect("lock");
    assert_eq!(
        channel.drain_pending(),
        vec![
            "have a look at the README too".to_string(),
            "a second look at the CHANGELOG".to_string()
        ],
        "handed to the model in arrival order"
    );
    assert!(
        channel.drain_pending().is_empty(),
        "what was taken is not repeated"
    );
    assert_eq!(
        channel.all().len(),
        2,
        "the transcript still shows everything"
    );
}

/// 0.16 follow-up: "after this turn" has a referent only while **a turn is running**.
#[tokio::test]
async fn follow_up_needs_a_turn_in_flight_and_counts_the_line() {
    let server = server(Arc::new(Mock::new(vec![])), false).await;

    assert!(
        matches!(
            server.follow_up("test", "I will do it later").await,
            crate::FollowUp::Idle
        ),
        "queueing while idle has no turn to wait on — the caller should just turn/start"
    );

    let (_interrupt, _steering) = server
        .begin_turn("t1", "test")
        .expect("the session is idle");
    assert!(matches!(
        server.follow_up("test", "the second thing").await,
        crate::FollowUp::Queued(1)
    ));
    assert!(matches!(
        server.follow_up("test", "the third thing").await,
        crate::FollowUp::Queued(2)
    ));
    assert_eq!(server.queue.count("test").await.expect("count"), 2);
    assert!(
        matches!(
            server.follow_up("other", "unrelated").await,
            crate::FollowUp::Idle
        ),
        "another session is not running, so this is unrelated"
    );
}

/// The promise of 0.16 follow-up: a queued message **becomes a turn of its own**, runs as
/// soon as the session is free, first in first out.
///
/// Two model calls per turn (the gate plus the reply), so two queued messages need four
/// scripted responses.
#[tokio::test]
async fn a_queued_message_becomes_its_own_turn_when_the_session_frees_up() {
    let mock = Mock::new(vec![
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "a small thing"}"#),
        Mock::text("the second thing is done."),
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "a small thing"}"#),
        Mock::text("the third thing is done."),
    ]);
    let server = server(Arc::new(mock), false).await;

    let (_interrupt, _steering) = server
        .begin_turn("t1", "test")
        .expect("the session is idle");
    server.follow_up("test", "the second thing").await;
    server.follow_up("test", "the third thing").await;

    // A turn ends: the equivalent of dropping the guard (Drop does finish + drain).
    server.finish_turn("t1");
    crate::turn::drain_queue(&server, "test").await;

    // Each message becomes its own turn and finishes on its own (the guard of the first drains
    // the second when it ends).
    let mut rows = Vec::new();
    for _ in 0..200 {
        rows = server
            .chat
            .messages("test", None, 50)
            .await
            .expect("read the conversation");
        let has_both = rows.iter().any(|row| row.content == "the second thing")
            && rows.iter().any(|row| row.content == "the third thing");
        if has_both && rows.len() >= 4 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let id_of = |text: &str| {
        rows.iter()
            .find(|row| row.content == text)
            .map(|row| row.id)
            .unwrap_or_else(|| panic!("a queued message never reached the conversation: {text}"))
    };
    assert!(
        id_of("the second thing") < id_of("the third thing"),
        "first in, first out"
    );
    assert_eq!(
        server.queue.count("test").await.expect("count"),
        0,
        "everything has run"
    );
    // A message reaching the history is not the same as the last turn having finished:
    // `finish_turn` releases the session **after** the write lands, and with the whole suite
    // running in parallel those two steps drift apart. Poll for it; do not bet on the order.
    let mut released = false;
    for _ in 0..200 {
        if server.active_turns(Some("test")).is_empty() {
            released = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(released, "a finished turn has to give the session back");
}

/// A turn finishes and the ledger has it: one row for the request, one for the reply,
/// fidelity exact, and the turn digest verifies.
///
/// This is the entry assertion for "rebuildable" — without it the ledger is just another
/// table nobody reads.
#[tokio::test]
async fn a_finished_turn_lands_in_the_ledger() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "small talk"}"#),
        Mock::text("Four."),
    ]));
    let server = server(mock, false).await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let sink = EventSink::new(tx);
    let _ = crate::turn::run_turn(
        &server,
        TurnStartParams {
            attachments: None,
            session_id: Some("test".to_string()),
            message: "What is 2+2?".to_string(),
            stream: Some(true),
            cwd: None,
            tier: None,
        },
        RequestId::Number(1),
        &sink,
    )
    .await;
    while rx.try_recv().is_ok() {}

    let turns = server
        .ledger
        .turns("test", 10)
        .await
        .expect("list the ledger");
    assert_eq!(turns.len(), 1, "one turn is one turn");
    assert_eq!(
        turns[0].fidelity, "exact",
        "this turn was recorded live, not backfilled"
    );
    assert!(
        turns[0].preview.contains("2+2"),
        "the preview has to be recognisable"
    );

    let entries = server
        .ledger
        .entries("test", &turns[0].turn_id)
        .await
        .expect("read the ledger");
    assert!(
        entries.iter().any(|entry| entry.kind == "user"),
        "the request has to be accounted for"
    );
    assert!(
        entries.iter().any(|entry| entry.kind == "assistant"),
        "the reply has to be accounted for"
    );
    assert!(
        server
            .ledger
            .verify("test", &turns[0].turn_id)
            .await
            .expect("verify the ledger"),
        "written just now, so verification has to pass"
    );
}

/// The system prompt carries the memory catalog but not the page bodies; an empty catalog
/// costs not a single word.
#[test]
fn the_system_prompt_carries_the_catalog_not_the_pages() {
    let catalog = "# Memory index\n\n2 pages · last updated 2026-11-12T00:00:00Z\n\n- [Alex](alex.md) · person — likes morning meetings\n";
    let with = crate::turn::build_system(crate::turn::PromptParts {
        soul: "You are Joy.",
        model: "test-model",
        provider: "mock",
        memory: "",
        memory_index: Some(catalog),
        skill_index: None,
        skills: "",
        todo: None,
        summary: None,
        instructions: None,
        capabilities: None,
    })
    .full();
    assert!(with.contains("Memory library"), "{with}");
    assert!(
        with.contains("[Alex](alex.md)"),
        "the catalog has to go in: {with}"
    );
    assert!(
        with.contains("read_memory_page"),
        "the model has to be told to read pages on demand: {with}"
    );
    assert!(
        !with.contains("Alice likes"),
        "page bodies must not ride along: {with}"
    );

    let without = crate::turn::build_system(crate::turn::PromptParts {
        soul: "You are Joy.",
        model: "test-model",
        provider: "mock",
        memory: "",
        memory_index: Some(""),
        skill_index: None,
        skills: "",
        todo: None,
        summary: None,
        instructions: None,
        capabilities: None,
    })
    .full();
    assert!(
        !without.contains("Memory library"),
        "an empty index takes up no prompt: {without}"
    );
    assert!(
        without.len() < with.len(),
        "the catalog is what makes it longer"
    );
}

/// The thin soul is for strong models: the same three rules without the scaffolding. Once
/// the user has touched the file, the file decides.
#[test]
fn the_thin_soul_trades_scaffolding_for_tokens_and_a_user_edit_wins() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let home = dir.path().to_path_buf();

    let thin = crate::turn::load_soul(&home, true);
    assert!(
        thin.contains("Concise, warm, proactive"),
        "the thin version has to keep the identity: {thin}"
    );
    assert!(
        !thin.contains("ask first"),
        "the thin version must not carry the long scaffolding's details: {thin}"
    );

    // The default text may swap with the mode — that is the "user never touched it" case.
    let full = crate::turn::load_soul(&home, false);
    assert!(
        full.contains("ask first"),
        "the full seed should carry the complete rules: {full}"
    );

    // A SOUL.md the user wrote is authoritative: no mode overwrites it.
    std::fs::write(home.join("SOUL.md"), "Call me captain and nothing else.")
        .expect("write SOUL.md");
    assert_eq!(
        crate::turn::load_soul(&home, true),
        "Call me captain and nothing else."
    );
    assert_eq!(
        crate::turn::load_soul(&home, false),
        "Call me captain and nothing else."
    );
}

/// The stable prefix and the changing tail have to stay apart: within one session the
/// prefix is byte-identical, and the timestamp goes down to the day only.
#[test]
fn the_stable_prefix_does_not_move_between_turns() {
    let parts = |todo: Option<&'static str>| crate::turn::PromptParts {
        soul: "You are Joy.",
        model: "test-model",
        provider: "mock",
        memory: "Alex likes morning meetings",
        memory_index: Some("- [Alex](alex.md) — person"),
        skill_index: Some("- demo — a workflow"),
        skills: "",
        todo,
        summary: None,
        instructions: None,
        capabilities: None,
    };
    let first = crate::turn::build_system(parts(None));
    let second = crate::turn::build_system(parts(Some("## To-do\n- [ ] write tests")));

    assert_eq!(
        first.stable, second.stable,
        "the prefix must be byte-identical across turns"
    );
    assert_ne!(
        first.tail, second.tail,
        "the to-do board changed, so the tail should change"
    );
    assert!(
        first.tail.contains("Right now it is"),
        "the date lives in the tail"
    );
    // Day granularity: hours and minutes would bust the cache once an hour.
    let now = chrono::Local::now();
    assert!(first.tail.contains(&now.format("%Y-%m-%d").to_string()));
    assert!(!first.tail.contains(&now.format("%H:%M").to_string()));
}

/// What this machine cannot do has to be said in the prompt.
///
/// Without that sentence the model cannot tell "this tool does not exist" from "I have not
/// found the right one yet" — one "clean up my calendar" request spent 4 of its 10 steps
/// reading this repository's docs. Codex puts its sandbox permissions in a developer
/// message, Claude Code puts the permission mode in the system prompt, for the same
/// reason: the boundary of what is possible belongs in the context, not in trial and error.
#[test]
fn the_prompt_says_what_the_machine_cannot_do() {
    let off = joyczl_config::Settings::default();
    assert!(
        !off.exec_enabled && !off.write_enabled,
        "both switches are off by default"
    );

    let note =
        crate::turn::capability_note(&off, true).expect("with everything off it has to appear");
    assert!(note.contains("JOY_EXEC"), "{note}");
    assert!(note.contains("JOY_WRITE"), "{note}");
    assert!(
        note.contains("say so"),
        "the point is \"if you cannot, say so\": {note}"
    );
    assert!(
        note.contains("`glob`") && note.contains("`grep`"),
        "no shell is not no search: {note}"
    );

    // One thing missing means one line about it: the note is not a fixed string, it follows
    // the switches.
    let half = joyczl_config::Settings {
        exec_enabled: true,
        ..joyczl_config::Settings::default()
    };
    let note = crate::turn::capability_note(&half, true).expect("writing files is missing");
    assert!(
        note.contains("JOY_WRITE") && !note.contains("JOY_EXEC"),
        "{note}"
    );
    assert!(
        !note.contains("no shell") && !note.contains("Searching is not"),
        "with the shell on, the searching aside would only confuse: {note}"
    );

    let full = joyczl_config::Settings {
        exec_enabled: true,
        write_enabled: true,
        ..joyczl_config::Settings::default()
    };
    assert!(
        crate::turn::capability_note(&full, true).is_none(),
        "with both on they must take up no prompt"
    );
}

// ---- Pre-loop steps must not outlast "stop" --------------------------------------------

#[tokio::test]
async fn a_stop_does_not_wait_for_a_slow_preloop_step() {
    // The gate/compaction are model calls made before the loop starts. Before this race, a
    // stop queued behind them; now it wins immediately.
    let cancel = joyczl_loop::Interrupt::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        trigger.cancel();
    });

    let started = std::time::Instant::now();
    let result = crate::turn::interruptible(
        Some(&cancel),
        "a long test step",
        tokio::time::sleep(std::time::Duration::from_secs(30)),
    )
    .await;

    assert!(
        result.is_none(),
        "an interrupted step should not return a result"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "a stop has to take effect at once, not after the step finishes: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn without_an_interrupt_the_step_just_runs() {
    let value = crate::turn::interruptible(None, "a test step", async { 7 }).await;
    assert_eq!(
        value,
        Some(7),
        "with no interrupt signal the behaviour is unchanged"
    );
}

// ---- Folded tool activity keeps an address, not just a memory --------------------------

fn outcome(name: &str, output: &str) -> joyczl_loop::ToolOutcome {
    joyczl_loop::ToolOutcome {
        tool_use_id: format!("tu-{name}"),
        name: name.to_string(),
        output: output.to_string(),
        duration_ms: 1,
    }
}

#[test]
fn a_folded_call_says_what_ran_how_much_and_the_first_line() {
    let folded = crate::turn::fold_tool_activity(
        "I have read it.",
        &[outcome("read_file", "first line\nsecond line\nthird line")],
    );
    assert!(folded.contains("read_file"), "{folded}");
    assert!(folded.contains("chars"), "{folded}");
    assert!(
        folded.contains("first line"),
        "the first line has to survive: {folded}"
    );
    assert!(
        !folded.contains("second line"),
        "only the first line is kept; the body comes back through read_file: {folded}"
    );
}

#[test]
fn a_spilled_result_keeps_the_address_to_read_it_again() {
    let output = "the first few lines\n…(90000 characters in total, truncated — 41 lines omitted; full output at spill/20261112/exec-101500-42.txt)\nthe last few lines";
    let folded =
        crate::turn::fold_tool_activity("done running.", &[outcome("run_command", output)]);
    assert!(
        folded.contains("spill/20261112/exec-101500-42.txt"),
        "after folding, where to fetch it again has to remain: {folded}"
    );
    assert!(folded.contains("run_command"), "{folded}");
}

#[test]
fn a_result_without_spill_does_not_invent_an_address() {
    let folded = crate::turn::fold_tool_activity("all right.", &[outcome("echo", "hi")]);
    assert!(
        !folded.contains("the full thing is at"),
        "the Chinese spill marker must be gone: {folded}"
    );
    assert!(folded.contains("hi"), "{folded}");
}

#[test]
fn no_tool_calls_leaves_the_reply_untouched() {
    assert_eq!(
        crate::turn::fold_tool_activity("just say one line", &[]),
        "just say one line",
        "nothing to fold leaves it alone"
    );
}

#[tokio::test]
async fn interrupted_queue_claim_is_visible_and_is_not_replayed() {
    let mock = Mock::new(Vec::new());
    let server = server(Arc::new(mock), false).await;
    server
        .queue
        .push("test", "uncertain instruction")
        .await
        .unwrap();
    let claim = server.queue.pop("test").await.unwrap().unwrap();
    server.restore_tasks().await;
    let rows = server.chat.messages("test", None, 50).await.unwrap();
    assert!(rows.iter().any(|row| row.content == claim.message));
    assert!(rows
        .iter()
        .any(|row| row.content.contains("outcome is unknown")));
    assert!(server.queue.pop("test").await.unwrap().is_none());
    assert!(server
        .queue
        .reconcile_interrupted()
        .await
        .unwrap()
        .is_empty());
    assert!(server.active_turns(Some("test")).is_empty());
}

#[tokio::test]
async fn quick_chat_keeps_recent_context_and_personality() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text(r#"{"route":"quick","reason":"conversation"}"#),
        Mock::text("I remember."),
    ]));
    let server = server(mock.clone(), true).await;
    std::fs::write(
        server.settings().home.join("SOUL.md"),
        "Be patient and warm.",
    )
    .unwrap();
    server
        .chat
        .append_exchange("My cat is Momo.", "How is Momo?", "test", "test", None)
        .await
        .unwrap();
    server
        .chat
        .append_exchange("private other session", "secret", "other", "test", None)
        .await
        .unwrap();
    let (sink, _receiver) = EventSink::channel();
    crate::turn::run_turn(
        &server,
        TurnStartParams {
            session_id: Some("test".into()),
            message: "Hello again".into(),
            ..Default::default()
        },
        RequestId::Number(1),
        &sink,
    )
    .await
    .unwrap();
    let requests = mock.received.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let quick = &requests[1];
    assert!(quick
        .system
        .as_deref()
        .unwrap()
        .contains("patient and warm"));
    let system = quick.system.as_deref().unwrap();
    assert!(system.contains("Current-message references are not remembered history."));
    assert!(system.contains("Do not promise continuous availability or guaranteed future recall."));
    assert_eq!(
        std::fs::read_to_string(server.settings().home.join("SOUL.md")).unwrap(),
        "Be patient and warm."
    );
    assert_eq!(quick.messages[0].text(), "My cat is Momo.");
    assert_eq!(quick.messages[1].text(), "How is Momo?");
    assert!(!quick
        .messages
        .iter()
        .any(|m| m.text().contains("private other session")));
}

#[tokio::test]
async fn quick_chat_retains_saved_preferences_without_retrieving_stale_or_project_claims() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text(r#"{"route":"quick","reason":"greeting"}"#),
        Mock::text("Welcome back."),
    ]));
    let server = server(mock.clone(), true).await;
    server
        .facts
        .add("user", "Please call me Lin.", "user", "user")
        .await
        .unwrap();
    server
        .facts
        .add("style", "Prefer gentle, short replies.", "user", "feedback")
        .await
        .unwrap();
    let (expired, _) = server
        .facts
        .add("mood", "obsolete mood marker", "user", "user")
        .await
        .unwrap();
    server
        .facts
        .set_validity(expired.id, None, Some("2000-01-01T00:00:00Z"))
        .await
        .unwrap();
    let (removed, _) = server
        .facts
        .add("user", "deleted preference marker", "user", "user")
        .await
        .unwrap();
    server.facts.delete(removed.id).await.unwrap();
    server
        .facts
        .add("project", "unrelated project marker", "user", "project")
        .await
        .unwrap();
    server
        .facts
        .add(
            "user",
            "unconfirmed inference marker",
            "consolidation",
            "user",
        )
        .await
        .unwrap();
    let (sink, _receiver) = EventSink::channel();
    crate::turn::run_turn(
        &server,
        TurnStartParams {
            session_id: Some("test".into()),
            message: "Hello again".into(),
            ..Default::default()
        },
        RequestId::Number(1),
        &sink,
    )
    .await
    .unwrap();
    let requests = mock.received.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "Personal context must not add a provider call"
    );
    let quick = &requests[1];
    assert!(quick.tools.is_empty());
    let memory = quick
        .messages
        .iter()
        .map(joyczl_provider::Message::text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(memory.contains("Please call me Lin."));
    assert!(memory.contains("Prefer gentle, short replies."));
    for excluded in [
        "obsolete mood marker",
        "deleted preference marker",
        "unrelated project marker",
        "unconfirmed inference marker",
    ] {
        assert!(
            !memory.contains(excluded),
            "Unexpected quick context: {excluded}"
        );
    }
}

#[tokio::test]
async fn quick_chat_still_answers_when_saved_preferences_are_unavailable() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text(r#"{"route":"quick","reason":"greeting"}"#),
        Mock::text("Still here."),
    ]));
    let server = server(mock.clone(), true).await;
    sqlx::query("DROP VIEW durable_facts")
        .execute(&server.pool)
        .await
        .unwrap();
    let (sink, _receiver) = EventSink::channel();
    let result = crate::turn::run_turn(
        &server,
        TurnStartParams {
            session_id: Some("test".into()),
            message: "Hello".into(),
            ..Default::default()
        },
        RequestId::Number(1),
        &sink,
    )
    .await;
    assert!(result.is_ok());
    let requests = mock.received.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].tools.is_empty());
    assert!(requests[1]
        .messages
        .last()
        .unwrap()
        .text()
        .contains("Hello"));
}
