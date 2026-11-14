//! Protocol-face tests for `joy mcp serve`: handshake, tool list, the behavior of the five
//! memory tools, and "which failures are RPC errors and which are text inside the result".

use serde_json::{json, Value};

use super::server::MemoryServer;

async fn server() -> MemoryServer {
    let dir = tempfile::tempdir().expect("temp dir");
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("open the store");
    let _ = dir.keep(); // sqlite still writes -wal/-shm: the dir must not be dropped here
    MemoryServer::new(
        joyczl_state::Facts::new(pool.clone()),
        joyczl_state::Episodes::new(pool),
    )
}

async fn call(server: &MemoryServer, method: &str, params: Value) -> Value {
    server
        .handle(json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
        .await
        .expect("a request with an id always gets a reply")
}

/// The text result of tools/call (content[0].text).
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
        "it has to declare the tools capability"
    );
    assert_eq!(result["serverInfo"]["name"], "joy-memory");
}

#[tokio::test]
async fn tools_list_exposes_only_the_memory_tools() {
    let server = server().await;
    let response = call(&server, "tools/list", json!({})).await;
    let names: Vec<&str> = response["result"]["tools"]
        .as_array()
        .expect("the tools array")
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
    // Memory only: no tool that "makes Joy act" ever appears.
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
               "arguments": {"subject": "alex", "content": "likes morning meetings"}}),
    )
    .await;
    assert!(text(&saved).contains("Remembered"), "{}", saved);

    let found = call(
        &server,
        "tools/call",
        json!({"name": "memory_search", "arguments": {"query": "alex"}}),
    )
    .await;
    assert!(
        text(&found).contains("likes morning meetings"),
        "what was written has to be findable: {found}"
    );
    // Source honestly marked as mcp — this fact was written by someone else's agent.
    assert_eq!(
        found["result"]["isError"],
        Value::Null,
        "this should not be an error"
    );
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
    assert!(text(&forgotten).contains("2 facts"), "{}", forgotten);

    let again = call(
        &server,
        "tools/call",
        json!({"name": "memory_forget", "arguments": {"subject": "bob"}}),
    )
    .await;
    assert!(
        text(&again).contains("no memory about"),
        "the second time has to say so plainly: {again}"
    );
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
        assert!(
            response["error"].is_null(),
            "{tool} on an empty store should not be an RPC error"
        );
        assert_eq!(response["result"]["isError"], Value::Null);
        assert!(
            text(&response).contains("Memory has no"),
            "{tool}: {response}"
        );
    }

    let search = call(
        &server,
        "tools/call",
        json!({"name": "memory_search", "arguments": {"query": "???"}}),
    )
    .await;
    assert!(
        text(&search).contains("Nothing in memory matches"),
        "{search}"
    );
}

#[tokio::test]
async fn a_notification_gets_no_response() {
    let server = server().await;
    let response = server
        .handle(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    assert!(
        response.is_none(),
        "a notification must not get a response: {response:?}"
    );
}

#[tokio::test]
async fn protocol_errors_and_tool_failures_take_different_paths() {
    let server = server().await;

    // Unknown tool: RPC error (the caller mistyped the name; that is a protocol-level fault).
    let unknown = call(
        &server,
        "tools/call",
        json!({"name": "nope", "arguments": {}}),
    )
    .await;
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");

    // Unknown method: also an RPC error, and it names the method.
    let method = call(&server, "turn/start", json!({})).await;
    assert_eq!(method["error"]["code"], -32601);
    assert!(method["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("turn/start"));

    // Missing args: text in the result + isError; the caller can read why.
    let bad_args = call(
        &server,
        "tools/call",
        json!({"name": "memory_search", "arguments": {}}),
    )
    .await;
    assert!(
        bad_args["error"].is_null(),
        "bad arguments should not be an RPC error"
    );
    assert_eq!(bad_args["result"]["isError"], true);
    assert!(text(&bad_args).contains("query"), "{bad_args}");
}
