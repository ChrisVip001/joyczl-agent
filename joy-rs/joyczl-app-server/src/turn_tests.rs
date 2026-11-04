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
    let interrupt = server.register_turn("t_live");
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
    assert_eq!(server.settings().max_iterations, 10);
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
    let png = [0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3, 4];
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
            std::fs::write(dir.join("px.png"), png).expect("装置图片");
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
    // 真实逃逸向量：uploads 里放一个指向 home 之外的符号链接，路径本身"存在"。
    let ran = run_with_attachments(
        Mock::new(vec![]),
        true,
        "看看这张图",
        png_attachment("uploads/escape.png"),
        |home| {
            let dir = home.join("uploads");
            std::fs::create_dir_all(&dir).expect("建上传目录");
            #[cfg(unix)]
            std::os::unix::fs::symlink("/etc/hosts", dir.join("escape.png"))
                .expect("建逃逸符号链接");
        },
    )
    .await;
    let error = ran.error.as_ref().expect("越界路径必须被拒");
    assert!(error.message.contains("home 之外"), "{}", error.message);
    assert!(ran.requests.is_empty());
}
