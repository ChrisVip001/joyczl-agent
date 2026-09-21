//! `joy mcp serve` 的协议面测试：握手、工具清单、五个记忆工具的行为，
//! 以及「哪些失败是 RPC 错误、哪些是结果里的文本」。

use serde_json::{json, Value};

use super::server::MemoryServer;

async fn server() -> MemoryServer {
    let dir = tempfile::tempdir().expect("临时目录");
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("打开库");
    std::mem::forget(dir); // sqlite 要写 -wal/-shm，目录不能提前消失
    MemoryServer::new(
        joyczl_state::Facts::new(pool.clone()),
        joyczl_state::Episodes::new(pool),
    )
}

async fn call(server: &MemoryServer, method: &str, params: Value) -> Value {
    server
        .handle(json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
        .await
        .expect("带 id 的请求必有应答")
}

/// tools/call 的文本结果（content[0].text）。
fn text(response: &Value) -> String {
    response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn initialize_hands_back_capabilities_and_identity() {
    let server = server().await;
    let response = call(&server, "initialize", json!({})).await;
    let result = &response["result"];
    assert_eq!(result["protocolVersion"], "2025-06-18");
    assert!(
        result["capabilities"]["tools"].is_object(),
        "要声明工具能力"
    );
    assert_eq!(result["serverInfo"]["name"], "joy-memory");
}

#[tokio::test]
async fn tools_list_exposes_only_the_memory_tools() {
    let server = server().await;
    let response = call(&server, "tools/list", json!({})).await;
    let names: Vec<&str> = response["result"]["tools"]
        .as_array()
        .expect("tools 数组")
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        names,
        vec![
            "memory_search",
            "memory_remember",
            "memory_forget",
            "memory_list",
            "memory_episodes"
        ]
    );
    // 只暴露记忆：不出现任何「让 Joy 去动手」的工具。
    assert!(!names
        .iter()
        .any(|n| n.contains("send") || n.contains("event")));
}

#[tokio::test]
async fn remember_then_search_round_trips_through_the_memory_tables() {
    let server = server().await;
    let saved = call(
        &server,
        "tools/call",
        json!({"name": "memory_remember",
               "arguments": {"subject": "alex", "content": "喜欢早上的会议"}}),
    )
    .await;
    assert!(text(&saved).contains("已记住"), "{}", saved);

    let found = call(
        &server,
        "tools/call",
        json!({"name": "memory_search", "arguments": {"query": "alex"}}),
    )
    .await;
    assert!(
        text(&found).contains("喜欢早上的会议"),
        "写进去的必须搜得到：{found}"
    );
    // 来源如实标注成 mcp —— 这条事实是从别人的 agent 写进来的。
    assert_eq!(found["result"]["isError"], Value::Null, "不该是错误");
}

#[tokio::test]
async fn forget_reports_the_count_and_says_when_there_is_nothing() {
    let server = server().await;
    call(
        &server,
        "tools/call",
        json!({"name": "memory_remember", "arguments": {"subject": "bob", "content": "a"}}),
    )
    .await;
    call(
        &server,
        "tools/call",
        json!({"name": "memory_remember", "arguments": {"subject": "bob", "content": "b"}}),
    )
    .await;

    let forgotten = call(
        &server,
        "tools/call",
        json!({"name": "memory_forget", "arguments": {"subject": "bob"}}),
    )
    .await;
    assert!(text(&forgotten).contains("2 条"), "{}", forgotten);

    let again = call(
        &server,
        "tools/call",
        json!({"name": "memory_forget", "arguments": {"subject": "bob"}}),
    )
    .await;
    assert!(text(&again).contains("没有"), "第二次要如实说：{again}");
}

#[tokio::test]
async fn empty_memory_answers_honestly_not_with_an_error() {
    let server = server().await;
    for (tool, args) in [("memory_list", json!({})), ("memory_episodes", json!({}))] {
        let response = call(
            &server,
            "tools/call",
            json!({"name": tool, "arguments": args}),
        )
        .await;
        assert!(response["error"].is_null(), "{tool} 空库不该是 RPC 错误");
        assert_eq!(response["result"]["isError"], Value::Null);
        assert!(text(&response).contains("还没有"), "{tool}: {response}");
    }

    let search = call(
        &server,
        "tools/call",
        json!({"name": "memory_search", "arguments": {"query": "???"}}),
    )
    .await;
    assert!(text(&search).contains("没有找到"), "{search}");
}

#[tokio::test]
async fn a_notification_gets_no_response() {
    let server = server().await;
    let response = server
        .handle(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    assert!(response.is_none(), "通知不该有应答：{response:?}");
}

#[tokio::test]
async fn protocol_errors_and_tool_failures_take_different_paths() {
    let server = server().await;

    // 不认识的工具：RPC 错误（调用方拼错了名字，这是协议层面的错）。
    let unknown = call(
        &server,
        "tools/call",
        json!({"name": "nope", "arguments": {}}),
    )
    .await;
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");

    // 不认识的方法：同样是 RPC 错误，且说得出名字。
    let method = call(&server, "turn/start", json!({})).await;
    assert_eq!(method["error"]["code"], -32601);
    assert!(method["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("turn/start"));

    // 参数没给够：结果里的文本 + isError，调用方读得到原因。
    let bad_args = call(
        &server,
        "tools/call",
        json!({"name": "memory_search", "arguments": {}}),
    )
    .await;
    assert!(bad_args["error"].is_null(), "参数错误不该是 RPC 错误");
    assert_eq!(bad_args["result"]["isError"], true);
    assert!(text(&bad_args).contains("query"), "{bad_args}");
}
