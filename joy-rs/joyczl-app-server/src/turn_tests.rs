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
        self.meta.as_ref().expect("跑完一轮必有 TurnCompleted")
    }
}

async fn server(mock: Arc<Mock>, graph_workflows: bool) -> Server {
    let dir = tempfile::tempdir().expect("临时目录");
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("打开库");
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
    last.expect("每个请求必有应答")
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
    let requests = mock.received.lock().expect("锁").clone();
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
        Mock::text("四。"),
    ]);
    let ran = run(mock, "2+2 等于几？", false).await;

    assert!(ran.error.is_none(), "{:?}", ran.error);
    let meta = ran.meta();
    assert!(meta.graph.is_none(), "图关着的时候不该留痕迹");
    assert!(meta.gate.is_some(), "普通路径有检索门");
    assert_eq!(meta.model, "test-model");
    assert_eq!(ran.reply, "四。");
    assert!(!has_graph_started(&ran), "没开图就没图的通知");
    assert_eq!(ran.requests.len(), 2, "门一次 + loop 一次");
    // Usage is persisted along with meta: token counts no longer live only in turnCompleted.
    assert_eq!(
        meta.usage,
        Some(TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
        })
    );
    assert!(!meta.interrupted, "没人打断它，meta 得说实话");
}

/// Quick answer: two model calls, one of them the small model's classification. **No retrieval gate**.
#[tokio::test]
async fn quick_route_is_one_short_call_and_no_gate() {
    let mock = Mock::new(vec![
        // Classifier: this line needs no memory lookup; the small model answers directly.
        Mock::text(r#"{"route": "quick", "reason": "greeting"}"#),
        Mock::text("你好呀。"),
    ]);
    let ran = run(mock, "你好", true).await;

    assert!(ran.error.is_none(), "{:?}", ran.error);
    let meta = ran.meta();
    let graph = meta.graph.as_ref().expect("走图就该有图的记录");
    assert_eq!(graph.workflow, "triage");
    assert_eq!(graph.route, GraphRouteKind::Quick);
    assert_eq!(graph.reason, "greeting");
    assert!(graph.path.contains(&"quick_reply".to_string()), "{graph:?}");

    assert_eq!(gate_of(&ran), None, "快答连记忆都不翻，哪来的门");
    assert_eq!(meta.iterations, 1);
    assert_eq!(meta.model, "test-small", "快答是小模型答的，meta 得说实话");
    assert_eq!(ran.reply, "你好呀。");

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
        "图有始有终"
    );
}

/// Full path: the gate is present and the big model still answers — the **same turn** as without the graph.
#[tokio::test]
async fn full_route_is_the_same_turn_as_the_plain_path() {
    let mock = Mock::new(vec![
        Mock::text(r#"{"route": "full", "reason": "about a person"}"#),
        Mock::text(r#"{"retrieve": true, "query": "阿明 会议", "reason": "about a person"}"#),
        Mock::text("你和阿明周四下午三点开会。"),
    ]);
    let ran = run(mock, "我什么时候和阿明开会？", true).await;

    assert!(ran.error.is_none(), "{:?}", ran.error);
    let meta = ran.meta();
    let graph = meta.graph.as_ref().expect("走图就该有图的记录");
    assert_eq!(graph.route, GraphRouteKind::Full);
    assert!(graph.path.contains(&"full_agent".to_string()), "{graph:?}");

    assert_eq!(
        gate_of(&ran),
        Some(joyczl_protocol::GateDecisionKind::Retrieve)
    );
    assert_eq!(meta.model, "test-model", "完整路径还是大模型");
    assert_eq!(ran.reply, "你和阿明周四下午三点开会。");
    // Classify + gate + loop.
    assert_eq!(ran.requests.len(), 3);
    assert!(
        ran.requests[1].messages[0]
            .text()
            .contains("retrieval gate"),
        "第二次调用是门，不是别的东西"
    );
}

/// A loop failure inside the graph: reported once, honestly, **no retry** — a retry would just fail the same way again.
#[tokio::test]
async fn a_failed_loop_inside_the_graph_is_reported_without_a_retry() {
    // The classifier said full, then nothing: the gate and the loop each hit the empty queue once.
    let mock = Mock::new(vec![Mock::text(r#"{"route": "full", "reason": "x"}"#)]);
    let ran = run(mock, "帮我看下", true).await;

    let error = ran.error.expect("该报错");
    assert_eq!(error.code, joyczl_protocol::codes::PROVIDER_ERROR);
    assert!(error.message.contains("模型调用失败"), "{error:?}");

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
    let dir = tempfile::tempdir().expect("临时目录");
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("打开库");
    std::fs::write(
        dir.path().join("mcp.json"),
        json!({"servers": [{"name": "ghost", "command": "/definitely/not/a/binary"}]}).to_string(),
    )
    .expect("写配置");

    let settings = Settings {
        home: dir.path().to_path_buf(),
        ..Settings::default()
    };
    let server = Server::boot(pool, settings).await;

    let names = server.tools.names();
    assert!(names.contains(&"save_note"), "{names:?}");
    assert!(
        !names.iter().any(|name| name.starts_with("ghost")),
        "连不上的服务器不该留下半截工具：{names:?}"
    );
}

/// Tools configured in `<home>/mcp.json` really land in the server's tool table — end to end.
///
/// The fake server is a real subprocess (`python3`, no SDK), consistent with the MCP side's approach:
/// what this layer verifies is that the "config → subprocess → tool table" line works end to end.
#[tokio::test]
async fn an_mcp_server_next_to_home_lands_in_the_tool_table() {
    let Some(python) = python3() else {
        eprintln!("跳过：这台机器上没有 python3");
        return;
    };
    let dir = tempfile::tempdir().expect("临时目录");
    let script = dir.path().join("fake_mcp_server.py");
    std::fs::write(&script, FAKE_SERVER).expect("写服务器");
    std::fs::write(
        dir.path().join("mcp.json"),
        json!({"servers": [{"name": "demo", "command": python, "args": [script]}]}).to_string(),
    )
    .expect("写配置");

    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("打开库");
    let settings = Settings {
        home: dir.path().to_path_buf(),
        ..Settings::default()
    };
    let server = Server::boot(pool, settings).await;

    let names = server.tools.names();
    assert!(
        names.contains(&"demo_echo"),
        "MCP 工具该带着服务器名前缀进表：{names:?}"
    );
    assert!(names.contains(&"current_time"), "本家工具还在：{names:?}");
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
        Mock::text("记好了。"),
    ]);
    let ran = run(mock, "帮我记点事", false).await;

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
        "ToolStarted 必须先于 ToolCompleted，且各只有一次"
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

    assert!(Arc::ptr_eq(&first, &again), "同一会话同一把锁");
    assert!(!Arc::ptr_eq(&first, &other), "不同会话各一把");
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
    assert!(blocked.is_err(), "写段被持有时，另一个获取者必须等");

    drop(guard);
    let relock = lock.clone();
    let acquired = tokio::time::timeout(std::time::Duration::from_millis(50), async move {
        let guard = relock.lock().await;
        drop(guard);
    })
    .await;
    assert!(acquired.is_ok(), "释放后另一个获取者立刻进来");
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
        other => panic!("打断一个不存在的 turn 不该报错：{other:?}"),
    }

    // A running turn: the response says it hit, and the token really flipped.
    let (interrupt, _) = server.begin_turn("t_live", "default").expect("会话空闲");
    let response = call(
        &server,
        request(2, "turn/interrupt", json!({"turnId": "t_live"})),
    )
    .await;
    match response {
        JsonRpcMessage::Response(resp) => {
            assert_eq!(resp.result, json!({"interrupted": true}))
        }
        other => panic!("打断在跑的 turn 不该报错：{other:?}"),
    }
    assert!(interrupt.is_cancelled(), "令牌必须真的取消");
    server.finish_turn("t_live");
}

/// config/write: takes effect in memory, lands on disk, survives a reboot — all three or nothing.
#[tokio::test]
async fn config_write_lands_in_memory_and_survives_a_reboot() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
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
        other => panic!("config/write 不该报错：{other:?}"),
    }
    // The next turn in memory uses the new values too.
    assert_eq!(server.settings().max_iterations, 20);
    assert_eq!(server.settings().max_tokens, 4096);
    // There is a record on disk.
    assert!(home.join("settings.json").exists(), "补丁必须落盘");

    // Reboot: open the same home again, patches stacked on top of env values.
    let settings2 = Settings {
        home,
        ..Settings::default()
    };
    let server2 = crate::open(&settings2).await.expect("重开");
    assert_eq!(server2.settings().max_iterations, 20);
    assert_eq!(server2.settings().max_tokens, 4096);
}

/// Models and keys are **not** on the config/write path: the patch has no provider / model / key fields;
/// changing them only goes through `model/*`. This assertion pins "the old road has been torn out".
#[tokio::test]
async fn config_write_cannot_smuggle_a_model_or_a_key_in() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
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
            json!({"patch": {"provider": "deepseek", "model": "deepseek-chat", "apiKey": "sk-漏进来的"}}),
        ),
    )
    .await;
    match response {
        JsonRpcMessage::Response(resp) => {
            // Unknown fields are ignored (not an error) — the point is that they **do not take effect**.
            assert_eq!(resp.result["config"]["provider"], "anthropic");
        }
        other => panic!("不该报错：{other:?}"),
    }
    assert_eq!(server.settings().provider, "anthropic");
    assert_eq!(server.settings().model, None);
    assert_eq!(server.settings().api_key, None);
    let saved = std::fs::read_to_string(home.join("settings.json")).unwrap_or_default();
    assert!(
        !saved.contains("sk-"),
        "密钥绝不能落到 settings.json：{saved}"
    );
}

/// config/write validation: a bad patch is rejected wholesale — half a patch is worse than none.
#[tokio::test]
async fn a_bad_patch_is_rejected_without_touching_anything() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
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
        other => panic!("非法补丁必须被拒绝：{other:?}"),
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
            let data = resp.result["data"].as_array().expect("data 数组");
            assert_eq!(data.len(), joyczl_provider::PROVIDERS.len());
            assert_eq!(data[0]["provider"], "anthropic");
            assert_eq!(data[0]["flagship"], "claude-sonnet-5");
        }
        other => panic!("model/list 不该报错：{other:?}"),
    }

    let response = call(
        &server,
        request(2, "model/list", json!({"provider": "deepseek"})),
    )
    .await;
    match response {
        JsonRpcMessage::Response(resp) => {
            let data = resp.result["data"].as_array().expect("data 数组");
            assert_eq!(data.len(), 1);
            assert_eq!(data[0]["provider"], "deepseek");
        }
        other => panic!("model/list 过滤不该报错：{other:?}"),
    }
}

/// After one turn: trace landed in traces/<date>.jsonl, usage landed in usage.jsonl —
/// observability is not optional, but a write failure must not drag the conversation down either.
#[tokio::test]
async fn a_turn_leaves_a_trace_and_a_usage_line_behind() {
    let mock = Mock::new(vec![
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "small talk"}"#),
        Mock::text("四。"),
    ]);
    let server = server(Arc::new(mock), false).await;
    let home = server.settings().home.clone();

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    crate::run_turn(
        &server,
        TurnStartParams {
            attachments: None,
            session_id: Some("test".to_string()),
            message: "2+2 等于几？".to_string(),
            stream: Some(true),
            cwd: None,
            tier: None,
        },
        RequestId::Number(1),
        &EventSink::new(tx),
    )
    .await
    .expect("这轮该跑通");

    // The trace file is named by date, one line per turn, each line carrying turnId / meta.
    let mut traces = std::fs::read_dir(home.join("traces"))
        .expect("traces 目录")
        .collect::<Vec<_>>();
    assert_eq!(traces.len(), 1, "一轮一个文件：{:?}", traces);
    let trace = std::fs::read_to_string(traces.remove(0).unwrap().path()).unwrap();
    let record: serde_json::Value = serde_json::from_str(trace.trim()).expect("合法 JSON");
    assert_eq!(record["userMessage"], "2+2 等于几？");
    assert_eq!(record["meta"]["model"], "test-model");
    assert_eq!(
        record["meta"]["usage"]["outputTokens"], 5,
        "usage 随 meta 落盘"
    );

    // The usage ledger: one line per turn.
    let usage = std::fs::read_to_string(home.join("usage.jsonl")).unwrap();
    let row: serde_json::Value = serde_json::from_str(usage.trim()).expect("合法 JSON");
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
        Ok(Mock::text("更早的对话摘要。")),
        // Second loop: success
        Ok(Mock::text("四。")),
    ]));
    let server = server(mock.clone(), false).await;
    // Accumulate a few turns of history — forced compaction needs something to fold.
    for i in 0..4 {
        server
            .chat
            .append_exchange(
                &format!("问题 {i}"),
                &format!("回答 {i}"),
                "test",
                "cli",
                None,
            )
            .await
            .expect("写会话");
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    crate::turn::run_turn(
        &server,
        TurnStartParams {
            attachments: None,
            session_id: Some("test".to_string()),
            message: "2+2 等于几？".to_string(),
            stream: Some(true),
            cwd: None,
            tier: None,
        },
        RequestId::Number(1),
        &EventSink::new(tx),
    )
    .await
    .expect("溢出后重试该跑通");

    let mut reply = String::new();
    while let Ok(frame) = rx.try_recv() {
        if let Frame::Notification(ServerNotification::TurnCompleted(done)) = frame {
            reply = done.reply.clone();
        }
    }
    assert_eq!(reply, "四。");
    assert_eq!(
        mock.received.lock().expect("锁").len(),
        4,
        "门 + 溢出 + 摘要 + 重试，一次都不多"
    );
    assert!(
        server
            .chat
            .load_rollup("test")
            .await
            .expect("查库")
            .is_some(),
        "强制压缩该留下摘要"
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
            message: "你好".to_string(),
            stream: Some(true),
            cwd: None,
            tier: None,
        },
        RequestId::Number(1),
        &EventSink::new(tx),
    )
    .await
    .expect_err("要如实报错");

    assert_eq!(error.code, joyczl_protocol::codes::PROVIDER_ERROR);
    assert_eq!(
        mock.received.lock().expect("锁").len(),
        2,
        "门一次 + loop 一次，不该有第二次模型调用"
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
        .expect("监听端口");
    let addr = listener.local_addr().expect("本地地址");
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
    serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": text}}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1},
    })
    .to_string()
}

/// One streaming response — the loop takes this path (`stream: true`).
fn sse_text(text: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({"choices": [{"delta": {"content": text}}]}),
        serde_json::json!({"choices": [], "usage": {"prompt_tokens": 2, "completion_tokens": 3}}),
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
        (200, sse_text("四。")),
    ])
    .await;

    let dir = tempfile::tempdir().expect("临时目录");
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
    let server = crate::open(&settings).await.expect("装配（走真实客户端）");

    let (tx, mut rx) = mpsc::unbounded_channel();
    crate::turn::run_turn(
        &server,
        TurnStartParams {
            attachments: None,
            session_id: Some("test".to_string()),
            message: "2+2 等于几？".to_string(),
            stream: Some(true),
            cwd: None,
            tier: None,
        },
        RequestId::Number(1),
        &EventSink::new(tx),
    )
    .await
    .expect("限流之后该照常跑完");

    let mut retries = 0;
    let mut meta_retries = -1;
    while let Ok(frame) = rx.try_recv() {
        if let Frame::Notification(notification) = frame {
            match notification {
                ServerNotification::Retry(retry) => {
                    retries += 1;
                    assert!(retry.reason.contains("429"), "{retry:?}");
                    assert!(!retry.turn_id.is_empty(), "通知要带上是哪一轮");
                }
                ServerNotification::TurnCompleted(done) => {
                    meta_retries = done.meta.retries;
                }
                _ => {}
            }
        }
    }
    assert_eq!(retries, 1, "每次重试都要有一条通知");
    assert_eq!(meta_retries, 1, "meta 里的次数是实测的");
}

/// Working-directory admission (fail-closed): valid ones pass; relative and nonexistent paths are blocked;
/// an empty string counts as not given — the same semantics as the protocol side's "default = process cwd".
#[test]
fn turn_cwd_validation_is_fail_closed() {
    use crate::turn::resolve_turn_cwd;

    let dir = tempfile::tempdir().expect("临时目录");
    let ok = resolve_turn_cwd(&Some(dir.path().display().to_string()));
    assert!(ok.unwrap().is_some(), "存在的绝对路径应该通过");

    assert!(resolve_turn_cwd(&None).unwrap().is_none());
    assert!(resolve_turn_cwd(&Some(String::new())).unwrap().is_none());
    assert!(resolve_turn_cwd(&Some("   ".into())).unwrap().is_none());

    // Relative path: rejected, and the error speaks plainly.
    let err = resolve_turn_cwd(&Some("相对/路径".into())).unwrap_err();
    assert!(err.message.contains("绝对路径"), "{}", err.message);

    // A nonexistent absolute path: rejected.
    let err = resolve_turn_cwd(&Some("/不存在/的/目录/joy".into())).unwrap_err();
    assert!(err.message.contains("不存在"), "{}", err.message);
}

// ---- 附件（多模态）：图片在进 provider 前被读成块、被 vision 闸门拦下 ----

/// 与 `run` 相同的流程，但带 vision 开关、附件列表和一个「启动前装置」回调（往 home 里写上传文件）。
async fn run_with_attachments(
    mock: Mock,
    vision: bool,
    message: &str,
    attachments: Option<Vec<joyczl_protocol::TurnAttachment>>,
    prepare: impl FnOnce(&std::path::Path),
) -> Ran {
    let mock = Arc::new(mock);
    let dir = tempfile::tempdir().expect("临时目录");
    prepare(dir.path());
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("打开库");
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
    let requests = mock.received.lock().expect("锁").clone();
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
    // 真 PNG 头（1×1）：本地要读得出宽高才放行 —— 12 字节的假签名会被判「读不出」。
    let png = {
        let mut bytes = vec![0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&13u32.to_be_bytes()); // IHDR 数据长
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    };
    let ran = run_with_attachments(
        Mock::new(vec![
            // 检索门控先烧一条（small model）。
            Mock::text(r#"{"retrieve": false, "query": "", "reason": "small talk"}"#),
            Mock::text("图里是一个色块。"),
        ]),
        true,
        "这张图是什么？",
        png_attachment("uploads/px.png"),
        |home| {
            let dir = home.join("uploads");
            std::fs::create_dir_all(&dir).expect("建上传目录");
            std::fs::write(dir.join("px.png"), png.clone()).expect("装置图片");
        },
    )
    .await;
    assert!(ran.error.is_none(), "{:?}", ran.error);
    // 检索门控的调用在前，主调用在后：扫全部请求找携带图片的那条用户消息。
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
        .expect("用户消息携带 Image 块");
    assert_eq!(image.0, "image/png", "类型来自扩展名嗅探");
    use base64::Engine as _;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(image.1)
            .expect("合法 base64"),
        png,
        "字节原样进 provider"
    );
    // 正文文本仍在（chat 历史 / 模型都能看到用户说了什么）。
    let with_text = ran
        .requests
        .iter()
        .flat_map(|r| r.messages.iter())
        .filter(|m| matches!(m.role, joyczl_provider::Role::User))
        .any(|m| m.text().contains("这张图是什么？"));
    assert!(with_text, "用户正文文本随图片一起进请求");
}

/// 长截图（1120×11100）字节数只有 2 MB 出头，旧的「20 MiB」闸门会放行，远端才拒 ——
/// 现在必须在发请求前拦下，并把真实尺寸说出来（远端只回一句 unsupported image）。
#[tokio::test]
async fn an_oversized_image_is_refused_with_its_real_dimensions() {
    let ran = run_with_attachments(
        Mock::new(vec![]),
        true,
        "看看这张长图",
        png_attachment("uploads/long.png"),
        |home| {
            let mut bytes = vec![0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
            bytes.extend_from_slice(&13u32.to_be_bytes());
            bytes.extend_from_slice(b"IHDR");
            bytes.extend_from_slice(&1120u32.to_be_bytes());
            bytes.extend_from_slice(&11_100u32.to_be_bytes());
            bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
            let dir = home.join("uploads");
            std::fs::create_dir_all(&dir).expect("建上传目录");
            std::fs::write(dir.join("long.png"), bytes).expect("装置图片");
        },
    )
    .await;
    let error = ran.error.as_ref().expect("超过单边上限必须在门口拦下");
    assert!(
        error.message.contains("1120×11100"),
        "报错要给出真实尺寸：{}",
        error.message
    );
    assert!(
        error.message.contains("分段"),
        "报错要给出可执行的下一步：{}",
        error.message
    );
    assert!(ran.requests.is_empty(), "失败不该烧掉任何模型调用");
}

#[tokio::test]
async fn attachments_fail_up_front_on_a_non_vision_provider() {
    let ran = run_with_attachments(
        Mock::new(vec![]),
        false,
        "看看这张图",
        png_attachment("uploads/px.png"),
        |_| {},
    )
    .await;
    let error = ran.error.as_ref().expect("vision 关着必须在门口拦下");
    assert!(
        error.message.contains("不支持图片输入"),
        "{}",
        error.message
    );
    assert!(ran.requests.is_empty(), "失败不该烧掉任何模型调用");
}

#[tokio::test]
async fn an_attachment_outside_home_is_refused_before_the_turn() {
    // 平台无关的逃逸向量：`..` 走出 home。文件必须真实存在 —— canonicalize 才分得清
    // 「不存在」与「越界」，断言才能钉住越界那一条错误。
    let ran = run_with_attachments(
        Mock::new(vec![]),
        true,
        "看看这张图",
        png_attachment("../escape.png"),
        |home| {
            std::fs::write(
                home.parent()
                    .expect("tempdir 必有父目录")
                    .join("escape.png"),
                [0u8; 8],
            )
            .expect("放逃逸文件");
        },
    )
    .await;
    let error = ran.error.as_ref().expect("越界路径必须被拒");
    assert!(error.message.contains("home 之外"), "{}", error.message);
    assert!(ran.requests.is_empty());
}

/// Unix 专属逃逸向量：符号链接指向 home 之外、路径本身「存在」。
/// Windows 上符号链接需要特权语义，同一个闸门由上面的 `../` 用例覆盖。
#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_attachment_outside_home_is_refused() {
    let ran = run_with_attachments(
        Mock::new(vec![]),
        true,
        "看看这张图",
        png_attachment("uploads/escape.png"),
        |home| {
            let dir = home.join("uploads");
            std::fs::create_dir_all(&dir).expect("建上传目录");
            std::os::unix::fs::symlink("/etc/hosts", dir.join("escape.png"))
                .expect("建逃逸符号链接");
        },
    )
    .await;
    let error = ran.error.as_ref().expect("越界路径必须被拒");
    assert!(error.message.contains("home 之外"), "{}", error.message);
    assert!(ran.requests.is_empty());
}

/// 0.15：`turn/start` 不带 cwd 时，用**会话自己记住**的那个目录 —— "工作目录跟着会话走"
/// 的全部实现就在这一个分支上，所以它值得一条自己的测试。
#[tokio::test]
async fn the_session_working_directory_is_used_when_the_turn_names_none() {
    // A real temp directory instead of a hardcoded `/tmp`: that path exists on Unix but not on a
    // Windows runner, so the fail-closed `session_cwd` (existing dir only) returned None there.
    let dir = tempfile::tempdir().expect("临时目录");
    let cwd = dir.path().to_string_lossy().to_string();
    let server = server(Arc::new(Mock::new(vec![])), false).await;
    server
        .chat
        .create_session("s1", None, Some(cwd.as_str()))
        .await
        .expect("建会话");

    assert_eq!(
        crate::turn::session_cwd(&server, "s1").await.as_deref(),
        Some(std::path::Path::new(cwd.as_str())),
        "会话记着的目录就是这一轮的可写范围根"
    );
    assert_eq!(
        crate::turn::session_cwd(&server, "s2").await,
        None,
        "没有这个会话时不该编一个目录出来"
    );
    assert_eq!(
        crate::turn::session_cwd(&server, "default").await,
        None,
        "没选过目录的会话就是没有目录（写入会被拒，而不是落到进程 cwd）"
    );
}

/// 目录在选中之后被删掉了：不能悄悄退回进程 cwd，那会让写入落在没人预期的地方。
#[tokio::test]
async fn a_vanished_session_directory_is_not_silently_replaced() {
    let server = server(Arc::new(Mock::new(vec![])), false).await;
    let work = tempfile::tempdir().expect("临时目录");
    let path = work.path().to_string_lossy().to_string();
    server
        .chat
        .create_session("s1", None, Some(&path))
        .await
        .expect("建会话");
    assert!(crate::turn::session_cwd(&server, "s1").await.is_some());

    drop(work);
    assert_eq!(
        crate::turn::session_cwd(&server, "s1").await,
        None,
        "目录不在了就是没有可写范围，不该拿别的目录顶上"
    );
}

/// `turn/list` 只报还在跑的：结束的那一轮必须消失，别人的会话不该被算进来。
#[tokio::test]
async fn turn_list_reports_only_what_is_still_running() {
    let server = server(Arc::new(Mock::new(vec![])), false).await;
    let _token = server.begin_turn("t1", "s1").expect("会话空闲");

    let all = server.active_turns(None);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].0, "t1");
    assert_eq!(all[0].1, "s1");
    assert!(server.active_turns(Some("s2")).is_empty(), "别的会话不串");
    assert_eq!(server.active_turns(Some("s1")).len(), 1);

    server.finish_turn("t1");
    assert!(server.active_turns(None).is_empty(), "跑完了就该消失");
}

/// 0.16 的第一半：**一个会话一次只跑一轮**。
///
/// 这条不变量以前不存在：两个 `turn/start` 会并行跑，写段交错、各自只看见一半历史。
#[tokio::test]
async fn one_turn_at_a_time_per_session() {
    let server = server(Arc::new(Mock::new(vec![])), false).await;

    let (_interrupt, _steering) = server.begin_turn("t1", "s1").expect("第一个该拿到会话");
    let holder = server
        .begin_turn("t2", "s1")
        .expect_err("同会话第二个必须被拒");
    assert_eq!(holder, "t1", "拒绝里要带住占用者的回合 id");

    assert!(server.begin_turn("t3", "s2").is_ok(), "别的会话不该被牵连");

    server.finish_turn("t1");
    assert!(server.begin_turn("t4", "s1").is_ok(), "上一轮结束后该放行");
}

/// 0.16 的第二半：补一句进的是**在跑的那一轮**；没有在跑的回合就明说，不静默变成新一轮。
#[tokio::test]
async fn steering_needs_a_running_turn_and_keeps_arrival_order() {
    let server = server(Arc::new(Mock::new(vec![])), false).await;

    assert!(
        server.steer("s1", "随便补一句").is_err(),
        "没有在跑的回合就该说不 —— 静默开新一轮是撒谎"
    );

    let (_interrupt, steering) = server.begin_turn("t1", "s1").expect("会话空闲");
    let (turn_id, queued) = server.steer("s1", "顺便看下 README").expect("有回合在跑");
    assert_eq!(turn_id, "t1");
    assert_eq!(queued, 1);
    let (_, queued) = server
        .steer("s1", "再看一眼 CHANGELOG")
        .expect("有回合在跑");
    assert_eq!(queued, 2);

    let mut channel = steering.lock().expect("锁");
    assert_eq!(
        channel.drain_pending(),
        vec![
            "顺便看下 README".to_string(),
            "再看一眼 CHANGELOG".to_string()
        ],
        "按到达顺序交给模型"
    );
    assert!(channel.drain_pending().is_empty(), "取过的不再重复");
    assert_eq!(channel.all().len(), 2, "转录仍然看得到全部");
}

/// 0.16 follow-up：只有**有回合在跑**时，「等这轮结束」才有对象。
#[tokio::test]
async fn follow_up_needs_a_turn_in_flight_and_counts_the_line() {
    let server = server(Arc::new(Mock::new(vec![])), false).await;

    assert!(
        matches!(
            server.follow_up("test", "等会儿做").await,
            crate::FollowUp::Idle
        ),
        "空闲时说「等这轮结束」没有对象 —— 应该让调用方直接 turn/start"
    );

    let (_interrupt, _steering) = server.begin_turn("t1", "test").expect("会话空闲");
    assert!(matches!(
        server.follow_up("test", "第二件事").await,
        crate::FollowUp::Queued(1)
    ));
    assert!(matches!(
        server.follow_up("test", "第三件事").await,
        crate::FollowUp::Queued(2)
    ));
    assert_eq!(server.queue.count("test").await.expect("数"), 2);
    assert!(
        matches!(
            server.follow_up("other", "无关").await,
            crate::FollowUp::Idle
        ),
        "别的会话没在跑，跟它无关"
    );
}

/// 0.16 follow-up 的承诺：排队的消息**自己变成一个回合**，会话空了就接着跑，先进先出。
///
/// 每一轮两次模型调用（检索门 + 回复），所以两条排队消息要备四份脚本。
#[tokio::test]
async fn a_queued_message_becomes_its_own_turn_when_the_session_frees_up() {
    let mock = Mock::new(vec![
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "小事"}"#),
        Mock::text("第二件事做完了。"),
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "小事"}"#),
        Mock::text("第三件事做完了。"),
    ]);
    let server = server(Arc::new(mock), false).await;

    let (_interrupt, _steering) = server.begin_turn("t1", "test").expect("会话空闲");
    server.follow_up("test", "第二件事").await;
    server.follow_up("test", "第三件事").await;

    // 一轮结束：守卫被丢掉的等价物（Drop 里就是 finish + drain）。
    server.finish_turn("t1");
    crate::turn::drain_queue(&server, "test").await;

    // 两条各自成回合、各自收尾（后一条由前一条的守卫在结束时排空）。
    let mut rows = Vec::new();
    for _ in 0..200 {
        rows = server
            .chat
            .messages("test", None, 50)
            .await
            .expect("读对话");
        let has_both = rows.iter().any(|row| row.content == "第二件事")
            && rows.iter().any(|row| row.content == "第三件事");
        if has_both && rows.len() >= 4 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let id_of = |text: &str| {
        rows.iter()
            .find(|row| row.content == text)
            .map(|row| row.id)
            .unwrap_or_else(|| panic!("排队的消息没进对话：{text}"))
    };
    assert!(id_of("第二件事") < id_of("第三件事"), "先进先出");
    assert_eq!(server.queue.count("test").await.expect("数"), 0, "都跑完了");
    // 消息进了历史 ≠ 最后一轮已经收尾：`finish_turn` 把会话放回去发生在落盘**之后**，
    // 全量并行跑测试时这两步会被拉开。轮询等它，别赌顺序。
    let mut released = false;
    for _ in 0..200 {
        if server.active_turns(Some("test")).is_empty() {
            released = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(released, "跑完就该把会话放回去");
}

/// 一轮跑完，账本里就有这一轮：请求、回复各一行，保真度 exact，轮摘要验得过。
///
/// 这是「可重建」的入口断言 —— 没有它，账本只是又一个没人读的表。
#[tokio::test]
async fn a_finished_turn_lands_in_the_ledger() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "small talk"}"#),
        Mock::text("四。"),
    ]));
    let server = server(mock, false).await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let sink = EventSink::new(tx);
    let _ = crate::turn::run_turn(
        &server,
        TurnStartParams {
            attachments: None,
            session_id: Some("test".to_string()),
            message: "2+2 等于几？".to_string(),
            stream: Some(true),
            cwd: None,
            tier: None,
        },
        RequestId::Number(1),
        &sink,
    )
    .await;
    while rx.try_recv().is_ok() {}

    let turns = server.ledger.turns("test", 10).await.expect("列账");
    assert_eq!(turns.len(), 1, "一轮就是一轮");
    assert_eq!(turns[0].fidelity, "exact", "这一轮是现场记的，不是回填");
    assert!(turns[0].preview.contains("2+2"), "预览要能认出这一轮");

    let entries = server
        .ledger
        .entries("test", &turns[0].turn_id)
        .await
        .expect("读账");
    assert!(
        entries.iter().any(|entry| entry.kind == "user"),
        "请求要入账"
    );
    assert!(
        entries.iter().any(|entry| entry.kind == "assistant"),
        "回复要入账"
    );
    assert!(
        server
            .ledger
            .verify("test", &turns[0].turn_id)
            .await
            .expect("验账"),
        "刚写完就该验得过"
    );
}

/// 系统提示里带「记忆库目录」，但不带正文；目录为空时一个字都不多占。
#[test]
fn the_system_prompt_carries_the_catalog_not_the_pages() {
    let catalog = "# 记忆目录\n\n2 页 · 最后更新 2026-11-12T00:00:00Z\n\n- [Alex](alex.md) · person — 喜欢早会\n";
    let with = crate::turn::build_system(crate::turn::PromptParts {
        soul: "你是 Joy。",
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
    assert!(with.contains("[Alex](alex.md)"), "目录要进去：{with}");
    assert!(
        with.contains("read_memory_page"),
        "要告诉模型按需读页：{with}"
    );
    assert!(!with.contains("Alice 喜欢"), "正文不该常驻：{with}");

    let without = crate::turn::build_system(crate::turn::PromptParts {
        soul: "你是 Joy。",
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
        "空目录不占提示：{without}"
    );
    assert!(without.len() < with.len(), "有目录才更长");
}

/// 薄 SOUL 是给强模型的：同样的三条纪律，去掉脚手架；用户动过文件之后，文件说了算。
#[test]
fn the_thin_soul_trades_scaffolding_for_tokens_and_a_user_edit_wins() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();

    let thin = crate::turn::load_soul(&home, true);
    assert!(thin.contains("简洁、温暖、主动"), "薄版该保留身份：{thin}");
    assert!(
        !thin.contains("先征得同意"),
        "薄版不该带长脚手架的细则：{thin}"
    );

    // 默认文本可以随模式互换 —— 那是「用户没动过」这一种情况。
    let full = crate::turn::load_soul(&home, false);
    assert!(full.contains("先征得同意"), "全版该有完整规则：{full}");

    // 用户写过的 SOUL.md 是权威：任何模式都不覆盖。
    std::fs::write(home.join("SOUL.md"), "只叫我船长。").expect("写 SOUL.md");
    assert_eq!(crate::turn::load_soul(&home, true), "只叫我船长。");
    assert_eq!(crate::turn::load_soul(&home, false), "只叫我船长。");
}

/// 稳定前缀与会变的尾巴必须分开：同一会话里前缀逐字节不变，时间只到日。
#[test]
fn the_stable_prefix_does_not_move_between_turns() {
    let parts = |todo: Option<&'static str>| crate::turn::PromptParts {
        soul: "你是 Joy。",
        model: "test-model",
        provider: "mock",
        memory: "Alex 喜欢早上的会议",
        memory_index: Some("- [Alex](alex.md) — 人"),
        skill_index: Some("- demo — 一段流程"),
        skills: "",
        todo,
        summary: None,
        instructions: None,
        capabilities: None,
    };
    let first = crate::turn::build_system(parts(None));
    let second = crate::turn::build_system(parts(Some("## 待办清单\n- [ ] 写测试")));

    assert_eq!(first.stable, second.stable, "前缀跨轮必须逐字节相同");
    assert_ne!(first.tail, second.tail, "待办变了，尾巴就该变");
    assert!(first.tail.contains("Right now it is"), "日期在尾巴里");
    // 日粒度：带时分就意味着每小时失效一次缓存。
    let now = chrono::Local::now();
    assert!(first.tail.contains(&now.format("%Y-%m-%d").to_string()));
    assert!(!first.tail.contains(&now.format("%H:%M").to_string()));
}

/// 本机缺什么要写进提示。
///
/// 没有这句时，模型分不清「这个工具不存在」和「我还没找到合适的工具」—— 一次「清理日历」
/// 的请求因此在 10 步里读了 4 篇本仓库文档。Codex 把自己的沙箱权限写进 developer 消息，
/// Claude Code 把权限模式写进系统提示，同一个理由：能力边界属于上下文，不属于试错。
#[test]
fn the_prompt_says_what_the_machine_cannot_do() {
    let off = joyczl_config::Settings::default();
    assert!(
        !off.exec_enabled && !off.write_enabled,
        "两个开关默认都是关的"
    );

    let note = crate::turn::capability_note(&off).expect("都关着就该出现");
    assert!(note.contains("JOY_EXEC"), "{note}");
    assert!(note.contains("JOY_WRITE"), "{note}");
    assert!(note.contains("say so"), "要说的是「做不到就直说」：{note}");

    // 只缺一样就只说一样：提示不是固定文案，它跟着开关走。
    let half = joyczl_config::Settings {
        exec_enabled: true,
        ..joyczl_config::Settings::default()
    };
    let note = crate::turn::capability_note(&half).expect("缺写文件");
    assert!(
        note.contains("JOY_WRITE") && !note.contains("JOY_EXEC"),
        "{note}"
    );

    let full = joyczl_config::Settings {
        exec_enabled: true,
        write_enabled: true,
        ..joyczl_config::Settings::default()
    };
    assert!(
        crate::turn::capability_note(&full).is_none(),
        "都开着就不该占提示"
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
        "测试用长步骤",
        tokio::time::sleep(std::time::Duration::from_secs(30)),
    )
    .await;

    assert!(result.is_none(), "被中断时不应返回步骤结果");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "停止应立即生效，不能等步骤跑完：{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn without_an_interrupt_the_step_just_runs() {
    let value = crate::turn::interruptible(None, "测试用步骤", async { 7 }).await;
    assert_eq!(value, Some(7), "没有 interrupt 信号时行为不变");
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
        "我看过了。",
        &[outcome("read_file", "第一行内容\n第二行\n第三行")],
    );
    assert!(folded.contains("read_file"), "{folded}");
    assert!(folded.contains("字"), "{folded}");
    assert!(folded.contains("第一行内容"), "{folded}");
    assert!(
        !folded.contains("第二行"),
        "只留首行，正文靠 read_file 重取：{folded}"
    );
}

#[test]
fn a_spilled_result_keeps_the_address_to_read_it_again() {
    let output = "头几行\n…（结果共 90000 字符，已截断，省略了 41 行；完整输出在 spill/20261112/exec-101500-42.txt）\n尾几行";
    let folded = crate::turn::fold_tool_activity("跑完了。", &[outcome("run_command", output)]);
    assert!(
        folded.contains("spill/20261112/exec-101500-42.txt"),
        "折叠后必须留下去哪儿重取：{folded}"
    );
    assert!(folded.contains("run_command"), "{folded}");
}

#[test]
fn a_result_without_spill_does_not_invent_an_address() {
    let folded = crate::turn::fold_tool_activity("好了。", &[outcome("echo", "hi")]);
    assert!(!folded.contains("全量在"), "{folded}");
    assert!(folded.contains("hi"), "{folded}");
}

#[test]
fn no_tool_calls_leaves_the_reply_untouched() {
    assert_eq!(crate::turn::fold_tool_activity("就说一句", &[]), "就说一句");
}
