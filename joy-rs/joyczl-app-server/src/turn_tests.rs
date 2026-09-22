//! 一轮 turn 的装配测试 —— 这一层只回答一个问题：**这一轮是怎么走的**。
//!
//! 模型是脚本化的假货（`joyczl_provider::mock`），"模型第几次说了什么" 完全
//! 可控，于是图的每条分支都能钉死。跑完看三样东西：
//!
//!   1. 客户端收到了哪些通知 —— 图的节点事件只在这一层看得见；
//!   2. `TurnMeta` 里的 gate / graph / model —— 这一轮的门和路线；
//!   3. **模型实际被调用了几次** —— 快答的全部价值就在这个数字上。
//!
//! 判据都写在这层：图**开着**时才存在的那条路，和不带图那条路，跑出来的
//! 东西必须只差「少了什么」，不能差「答错了什么」。

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

/// 一次 turn 跑完之后，测试要看的全部东西。
struct Ran {
    notifications: Vec<ServerNotification>,
    /// 快答与完整两条路都会发 `TurnCompleted`；只有提前失败才没有。
    meta: Option<TurnMeta>,
    reply: String,
    /// 模型收到的请求，按顺序。这个向量就是「花了多少钱」。
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
    // 目录跟着测试进程走 —— 临时目录是这里的脚手架，不是被测对象。
    let _ = dir.keep(); // sqlite 还要写 -wal/-shm：目录不能在这里被删掉

    let server = Server::boot(pool, settings).await;
    // 真 provider 要 key，这里换成脚本化的假货：同一条 trait 上的另一个实现，
    // 这也正是把「模型调用」收敛成一个 trait 之后白拿的测试能力。
    server.set_resolved(Some(Resolved {
        provider_id: "mock".to_string(),
        client: mock,
        model: "test-model".to_string(),
        small_model: "test-small".to_string(),
    }));
    server
}

/// 发一条请求，等它的应答（通知排空即丢 —— 专测应答的方法用）。
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
            session_id: Some("test".to_string()),
            message: message.to_string(),
            stream: Some(true),
        },
        RequestId::Number(1),
        &sink,
    )
    .await;

    // 通知和应答走同一条通道，应答是最后一条 —— 排空即全部到齐。
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

    // 先把收到的请求抄出来再组 Ran —— 锁的守卫活不到函数尾表达式。
    let requests = mock.received.lock().expect("锁").clone();
    Ran {
        notifications,
        meta,
        reply,
        requests,
        error: outcome.err(),
    }
}

/// 客户端看到的节点顺序。
fn nodes(ran: &Ran) -> Vec<String> {
    ran.notifications
        .iter()
        .filter_map(|notification| match notification {
            ServerNotification::GraphNodeStarted(started) => Some(started.node.clone()),
            _ => None,
        })
        .collect()
}

/// 检索门这次说了什么 —— `None` 表示这一轮根本没进门。
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

/// 关掉图 = 今天的行为，一个字都不差。
#[tokio::test]
async fn graph_off_is_the_plain_path() {
    let mock = Mock::new(vec![
        // 检索门：不用查记忆。
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "small talk"}"#),
        // THE LOOP。
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
    // usage 随 meta 落库：token 数不再只活在 turnCompleted 里。
    assert_eq!(
        meta.usage,
        Some(TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
        })
    );
    assert!(!meta.interrupted, "没人打断它，meta 得说实话");
}

/// 快答：两次模型调用，其中一次是小模型的分类。**没有检索门**。
#[tokio::test]
async fn quick_route_is_one_short_call_and_no_gate() {
    let mock = Mock::new(vec![
        // 分类器：这句不用查记忆，小模型直接答。
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

    // 两次：分类一次、快答一次。没有第三次 —— 这正是快答便宜的全部。
    assert_eq!(ran.requests.len(), 2);
    assert!(ran.requests[0].messages[0].text().contains("triage gate"));

    // 图的通知发了，节点顺序与 meta 里记的对得上。
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

/// 完整路径：门在，答的还是大模型 —— 跟不带图时**同一轮**。
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
    // 分类 + 门 + loop。
    assert_eq!(ran.requests.len(), 3);
    assert!(
        ran.requests[1].messages[0]
            .text()
            .contains("retrieval gate"),
        "第二次调用是门，不是别的东西"
    );
}

/// 图里的 loop 失败：如实报一次，**不重试** —— 重试只会用同样的方式再失败。
#[tokio::test]
async fn a_failed_loop_inside_the_graph_is_reported_without_a_retry() {
    // 分类说了 full，然后就没了：门和 loop 各撞一次空队列。
    let mock = Mock::new(vec![Mock::text(r#"{"route": "full", "reason": "x"}"#)]);
    let ran = run(mock, "帮我看下", true).await;

    let error = ran.error.expect("该报错");
    assert_eq!(error.code, joyczl_protocol::codes::PROVIDER_ERROR);
    assert!(error.message.contains("模型调用失败"), "{error:?}");

    // 三次尝试：分类、门、loop。多出来一次就说明重试了。
    assert_eq!(ran.requests.len(), 3);
    // 图本身是正常收尾的（节点错只记录不外抛）—— 失败的是里面那次 loop。
    assert_eq!(
        ran.notifications
            .iter()
            .filter(|n| matches!(n, ServerNotification::GraphEnded(_)))
            .count(),
        1
    );
}

/// 配错的 MCP 服务器不该让整个助理起不来。
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

/// `<home>/mcp.json` 里配的工具真的进了服务端的工具表 —— 端到端。
///
/// 假服务器是个真子进程（`python3`，没有 SDK），跟 MCP 那边的做法一致：
/// 这一层要验的就是「配置 → 子进程 → 工具表」这条线通到底。
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

/// 一个只会 initialize + tools/list 的 MCP 服务器。
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

/// 工具开始的通知必须赶在工具完成的通知**前面** —— 客户端才能画出
/// "正在调用 X"。这是 turn/interrupt 之外 ToolStarted 存在的全部理由。
#[tokio::test]
async fn tool_started_goes_out_before_tool_completed() {
    let mock = Mock::new(vec![
        // 检索门。
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

/// turn/interrupt：在跑的 turn 拨得下；没在跑的 turn 打断不到 —— 但要
/// 诚实地说 false，而不是装作打断了什么。
#[tokio::test]
async fn interrupt_hits_a_running_turn_and_misses_a_finished_one() {
    let mock = Arc::new(Mock::new(vec![]));
    let server = server(mock, false).await;

    // 没登记过的 turn。
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

    // 在跑的 turn：应答说打到了，令牌也真的拨下了。
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

/// config/write：内存生效、磁盘落档、重启还在 —— 三样缺一不可。
#[tokio::test]
async fn config_write_lands_in_memory_and_survives_a_reboot() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    let _ = dir.keep(); // sqlite 还要写 -wal/-shm：目录不能在这里被删掉
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
            json!({"patch": {"maxIterations": 20, "provider": "deepseek"}}),
        ),
    )
    .await;
    match response {
        JsonRpcMessage::Response(resp) => {
            assert_eq!(resp.result["config"]["maxIterations"], 20);
            assert_eq!(resp.result["config"]["provider"], "deepseek");
        }
        other => panic!("config/write 不该报错：{other:?}"),
    }
    // 内存里的下一轮也用新值。
    assert_eq!(server.settings().max_iterations, 20);
    assert_eq!(server.settings().provider, "deepseek");
    // 磁盘上有档。
    assert!(home.join("settings.json").exists(), "补丁必须落盘");

    // 重启：同一个 home 再开一次，补丁叠在环境值上。
    let settings2 = Settings {
        home,
        ..Settings::default()
    };
    let server2 = crate::open(&settings2).await.expect("重开");
    assert_eq!(server2.settings().max_iterations, 20);
    assert_eq!(server2.settings().provider, "deepseek");
}

/// config/write 的校验：写坏了就整个拒绝，半个补丁比没有更糟。
#[tokio::test]
async fn a_bad_patch_is_rejected_without_touching_anything() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    let _ = dir.keep(); // sqlite 还要写 -wal/-shm：目录不能在这里被删掉
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
    // 什么都没改：内存还是默认值，盘上没有补丁文件。
    assert_eq!(server.settings().max_iterations, 10);
    assert!(!home.join("settings.json").exists());
}

/// model/list：目录就是 PROVIDERS 那张表，filter 按 provider 收窄。
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

/// 跑完一轮：trace 落了 traces/<日期>.jsonl，usage 落了 usage.jsonl ——
/// 观测不是可选项，但写失败也不该连累对话。
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
            session_id: Some("test".to_string()),
            message: "2+2 等于几？".to_string(),
            stream: Some(true),
        },
        RequestId::Number(1),
        &EventSink::new(tx),
    )
    .await
    .expect("这轮该跑通");

    // trace 文件按日期命名，每轮一行，行里带 turnId / meta。
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

    // usage 账本：一轮一行。
    let usage = std::fs::read_to_string(home.join("usage.jsonl")).unwrap();
    let row: serde_json::Value = serde_json::from_str(usage.trim()).expect("合法 JSON");
    assert_eq!(row["provider"], "mock");
    assert_eq!(row["inputTokens"], 10);
    assert_eq!(row["iterations"], 1);
}

// ---- 上下文溢出：压缩后重试一次 ---------------------------------------------

/// 溢出是唯一值得重试的错误：压一次再试，成功就照常收尾。
#[tokio::test]
async fn a_context_overflow_is_compacted_and_retried_exactly_once() {
    let mock = Arc::new(Mock::with_outcomes(vec![
        // 门
        Ok(Mock::text(
            r#"{"retrieve": false, "query": "", "reason": "small talk"}"#,
        )),
        // 第一次 loop：provider 说上下文超了
        Mock::context_overflow(),
        // 重试前的强制压缩（摘要那一次模型调用）
        Ok(Mock::text("更早的对话摘要。")),
        // 第二次 loop：成功
        Ok(Mock::text("四。")),
    ]));
    let server = server(mock.clone(), false).await;
    // 攒几轮历史 —— 强制压缩得有东西可折。
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
            session_id: Some("test".to_string()),
            message: "2+2 等于几？".to_string(),
            stream: Some(true),
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

/// 别的错误**不**重试：重试只会用同样的方式再失败一次（白等一倍时间）。
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
            session_id: Some("test".to_string()),
            message: "你好".to_string(),
            stream: Some(true),
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

// ---- 限流重试：真 HTTP、真客户端 --------------------------------------------

/// 把一次请求**读完**（头部 + `Content-Length` 指的 body）再回话。
///
/// 不等读完就回、然后关连接，会把还在写 body 的客户端顶掉 —— macOS/Linux 上
/// 多半没事，Windows 上表现为 RST，客户端那边看到的是
/// 「error sending request」，于是「服务器明明回了」变成一条网络错误。
/// 这个测试的请求带着整份 system prompt 与工具表，body 超过一次 read 的量，
/// 所以它只在 Windows 上稳定复现。
async fn drain_request(socket: &mut tokio::net::TcpStream) {
    // 导入写在函数里：这两个测试文件各自在别处按需 use，别处不引这个 trait。
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

/// 一个极小的 HTTP 服务器：按剧本逐个应答。返回 base_url。
///
/// 这里不引测试框架也不用 Mock —— 要钉住的正是「真的 openai 客户端在真的
/// 429 上会退避重试」，用假客户端就测不到这件事。
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
            // `Retry-After: 0`：既验证读了它，又让测试不用真等。
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

/// 一次完整的（非流式）应答 —— 门走这条路。
fn completion(text: &str) -> String {
    serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": text}}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1},
    })
    .to_string()
}

/// 一次流式应答 —— loop 走这条路（`stream: true`）。
fn sse_text(text: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({"choices": [{"delta": {"content": text}}]}),
        serde_json::json!({"choices": [], "usage": {"prompt_tokens": 2, "completion_tokens": 3}}),
    )
}

/// 429 被重试，而且重试**看得到**：一条 Retry 通知 + `meta.retries`。
#[tokio::test]
async fn a_rate_limited_call_is_retried_and_reported_in_the_meta() {
    let base = scripted_http(vec![
        (429, r#"{"error":{"message":"slow down"}}"#.to_string()),
        // 门（重试之后才轮到它）
        (
            200,
            completion(r#"{"retrieve": false, "query": "", "reason": "small talk"}"#),
        ),
        // loop
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
            session_id: Some("test".to_string()),
            message: "2+2 等于几？".to_string(),
            stream: Some(true),
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
