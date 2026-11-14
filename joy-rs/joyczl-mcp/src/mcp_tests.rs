//! Tests for the MCP client.
//!
//! Three layers:
//!   1. Config decisions and renaming — pure functions, no process, no network.
//!   2. The two wire framings — in-memory pipes (stdio) and a local fake HTTP server.
//!   3. A real subprocess server (a fake MCP written in `python3`, no SDK),
//!      end-to-end up to registering into ToolRegistry and calling successfully.
//!
//! Layer 3 needs python3; skip if the machine lacks it (never fake a pass); `scripts/smoke.sh`
//! also carries an end-to-end of the same flavor.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::config::{self, Config, ServerSpec, Transport as TransportSpec};
use crate::transport::{HttpSession, StdioSession};
use crate::McpClient;

fn no_env(_key: &str) -> Option<String> {
    None
}

// ---- 1. Config -------------------------------------------------------------

#[test]
fn model_safe_name_keeps_what_the_provider_accepts() {
    assert_eq!(
        config::model_safe_name("notes", "remember"),
        "notes_remember"
    );
    // Dots are common in MCP, but vendors' `^[a-zA-Z0-9_-]{1,64}$` rejects them.
    assert_eq!(
        config::model_safe_name("team-memory", "memory.remember"),
        "team-memory_memory_remember"
    );
    let long = config::model_safe_name("s", &"x".repeat(200));
    assert_eq!(long.len(), 64);
    assert!(
        !long.starts_with('_'),
        "a truncated name must not start with _: {long}"
    );
}

#[test]
fn config_refuses_two_transports_and_two_credentials() {
    let both_transports = ServerSpec {
        name: "x".to_string(),
        url: Some("https://h/mcp".to_string()),
        command: Some("npx".to_string()),
        ..ServerSpec::default()
    };
    assert!(config::resolve(&both_transports, &no_env)
        .unwrap_err()
        .contains("pick one transport"));

    let both_creds = ServerSpec {
        name: "x".to_string(),
        url: Some("https://h/mcp".to_string()),
        auth_env: Some("K".to_string()),
        oauth: Some(true),
        ..ServerSpec::default()
    };
    assert!(config::resolve(&both_creds, &no_env)
        .unwrap_err()
        .contains("pick one"));

    let neither = ServerSpec {
        name: "x".to_string(),
        ..ServerSpec::default()
    };
    assert!(config::resolve(&neither, &no_env).is_err());
}

#[test]
fn oauth_resolves_to_anonymous_http_the_token_layer_sits_above() {
    // `oauth: true` becomes "anonymous Http" at the resolve layer: the real bearer is
    // fetched by lib.rs from mcp-auth's token store (see oauth.rs); if never signed in,
    // it reports "go run joy mcp login".
    let spec = ServerSpec {
        name: "x".to_string(),
        url: Some("https://h/mcp".to_string()),
        oauth: Some(true),
        ..ServerSpec::default()
    };
    match config::resolve(&spec, &no_env).expect("oauth is a legal config") {
        config::Transport::Http { token, .. } => assert!(token.is_none()),
        other => panic!("oauth should take the Http transport: {other:?}"),
    }
}

#[test]
fn a_named_but_unset_variable_is_refused_before_connecting() {
    // Connecting anonymously would only yield a 401 and "tools vanished", reading like a dead server.
    let spec = ServerSpec {
        name: "x".to_string(),
        url: Some("https://h/mcp".to_string()),
        auth_env: Some("NOTES_API_KEY".to_string()),
        ..ServerSpec::default()
    };
    let message = config::resolve(&spec, &no_env).unwrap_err();
    assert!(message.contains("NOTES_API_KEY"), "{message}");

    let found = config::resolve(&spec, &|key| {
        (key == "NOTES_API_KEY").then(|| "s3cret".to_string())
    })
    .expect("with a value it connects");
    assert_eq!(
        found,
        TransportSpec::Http {
            url: "https://h/mcp".to_string(),
            token: Some("s3cret".to_string()),
        }
    );
}

#[test]
fn stdio_and_http_are_chosen_by_shape() {
    let stdio = ServerSpec {
        name: "fs".to_string(),
        command: Some("npx".to_string()),
        args: Some(vec!["-y".to_string(), "server".to_string()]),
        env: Some(BTreeMap::from([("A".to_string(), "1".to_string())])),
        ..ServerSpec::default()
    };
    assert_eq!(
        config::resolve(&stdio, &no_env).expect("stdio"),
        TransportSpec::Stdio {
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "server".to_string()],
            env: Some(BTreeMap::from([("A".to_string(), "1".to_string())])),
        }
    );

    let anonymous = ServerSpec {
        name: "pub".to_string(),
        url: Some("https://h/mcp".to_string()),
        ..ServerSpec::default()
    };
    assert_eq!(
        config::resolve(&anonymous, &no_env).expect("http"),
        TransportSpec::Http {
            url: "https://h/mcp".to_string(),
            token: None,
        }
    );
}

#[test]
fn the_auth_hint_names_what_the_caller_can_check() {
    let anonymous = TransportSpec::Http {
        url: "https://h/mcp".to_string(),
        token: None,
    };
    let hint = config::auth_hint(&anonymous).expect("it should offer a hint");
    assert!(hint.contains("https://h/mcp"));
    assert!(hint.contains("auth_env"));
    // A local subprocess has no such thing as auth.
    let stdio = TransportSpec::Stdio {
        command: "npx".to_string(),
        args: vec![],
        env: None,
    };
    assert!(config::auth_hint(&stdio).is_none());
}

#[test]
fn config_parses_the_documented_shape() {
    let config = Config::parse(
        r#"{"servers": [{"name": "fs", "command": "npx", "args": ["-y", "s"]},
                        {"name": "notes", "url": "https://h/mcp", "auth_env": "K"}]}"#,
    )
    .expect("it resolves");
    assert_eq!(config.servers.len(), 2);
    assert_eq!(config.servers[0].command.as_deref(), Some("npx"));
    assert_eq!(config.servers[1].auth_env.as_deref(), Some("K"));
    assert!(Config::parse("not json").is_err());
}

#[tokio::test]
async fn no_config_file_means_no_mcp() {
    let client = McpClient::connect(std::path::Path::new("/nope/mcp.json")).await;
    assert!(client.servers().is_empty());
    assert!(
        client.warnings().is_empty(),
        "unconfigured is not a mistake"
    );
    assert!(client.tools().is_empty());
}

#[tokio::test]
async fn a_broken_config_file_says_so() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("mcp.json");
    std::fs::write(&path, "{ not json").expect("write");
    let client = McpClient::connect(&path).await;
    assert!(client.servers().is_empty());
    assert_eq!(client.warnings().len(), 1);
    assert!(client.warnings()[0].contains("mcp.json"));
}

/// `config_paths_for`: the user's file first, then the extras (colon-separated on
/// unix, semicolon on Windows — hence `join_paths` instead of a literal ":").
/// The extras are what makes "built-in" real: the dashboard greys them out.
#[test]
fn config_paths_put_the_users_file_first() {
    let home = std::path::Path::new("/home/u/.joy");
    assert_eq!(
        config::config_paths_for(home, None),
        vec![std::path::PathBuf::from("/home/u/.joy/mcp.json")],
        "with no extra directory, only the user's own is read"
    );

    let extra =
        std::env::join_paths(["/etc/joy/mcp.json", "/opt/pack/mcp.json"]).expect("the paths join");
    assert_eq!(
        config::config_paths_for(home, Some(&extra)),
        vec![
            std::path::PathBuf::from("/home/u/.joy/mcp.json"),
            std::path::PathBuf::from("/etc/joy/mcp.json"),
            std::path::PathBuf::from("/opt/pack/mcp.json"),
        ],
        "the user's comes first — read first, so it wins on a name clash"
    );
}

/// A shared pack re-declaring a server the user already has must not replace it:
/// first file wins, and the loser is skipped rather than double-registered.
/// (The extras' own broken entries still warn — one bad pack takes nothing down.)
#[tokio::test]
async fn the_users_config_file_wins_over_the_extras() {
    let fake = fake_http(vec![
        // Handshake
        Reply {
            status: 200,
            content_type: "application/json",
            body: r#"{"jsonrpc":"2.0","id":@id,"result":{"protocolVersion":"2025-06-18"}}"#
                .to_string(),
        },
        // The handshake's second leg: the 202 of `notifications/initialized`
        Reply {
            status: 202,
            content_type: "application/json",
            body: String::new(),
        },
        // tools/list
        Reply {
            status: 200,
            content_type: "application/json",
            body: r#"{"jsonrpc":"2.0","id":@id,"result":{"tools":[{"name":"reverse"}]}}"#
                .to_string(),
        },
    ])
    .await;

    let dir = tempfile::tempdir().expect("temp dir");
    let user = dir.path().join("mcp.json");
    std::fs::write(
        &user,
        json!({"servers": [{"name": "demo", "url": fake.url}]}).to_string(),
    )
    .expect("write the user config");
    let extra = dir.path().join("pack.json");
    std::fs::write(
        &extra,
        json!({"servers": [
            {"name": "demo", "command": "definitely-not-a-command-xyz"},
            {"name": "other", "command": "definitely-not-a-command-xyz"}
        ]})
        .to_string(),
    )
    .expect("write the extra config");

    let client = McpClient::connect_paths(&[user, extra]).await;
    assert_eq!(
        client.servers(),
        vec!["demo"],
        "the local one survives; the duplicate in the extra file is skipped and does not displace it"
    );
    assert_eq!(
        client.warnings().len(),
        2,
        "the two ghosts in the extra file each leave one warning line: {:?}",
        client.warnings()
    );
}

// ---- 2. Wire framings ------------------------------------------------------

/// A stdio peer that only answers per script, running on in-memory pipes.
#[tokio::test]
async fn stdio_session_frames_json_lines_and_skips_foreign_messages() {
    let (client_side, server_side) = tokio::io::duplex(8192);
    let (server_read, mut server_write) = tokio::io::split(server_side);
    let server = tokio::spawn(async move {
        let mut lines = BufReader::new(server_read).lines();
        while let Some(line) = lines.next_line().await.expect("read") {
            let message: serde_json::Value = serde_json::from_str(&line).expect("JSON");
            let method = message["method"].as_str().unwrap_or("").to_string();
            let Some(id) = message.get("id").cloned() else {
                continue; // notification, no reply
            };
            let result = match method.as_str() {
                "initialize" => json!({"protocolVersion": "2025-06-18"}),
                "tools/list" => {
                    // Slip one notification in before our answer: the client must skip it.
                    server_write
                        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n")
                        .await
                        .expect("write");
                    json!({"tools": [{"name": "reverse"}]})
                }
                _ => json!({}),
            };
            let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
            server_write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .expect("write");
        }
    });

    let (client_read, client_write) = tokio::io::split(client_side);
    let mut session = StdioSession::new(client_read, client_write);
    session.initialize().await.expect("handshake");
    let tools = session.list_tools().await.expect("list the tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "reverse");
    // No schema from the server: degrade to "an object with no parameters".
    assert_eq!(
        tools[0].schema(),
        json!({"type": "object", "properties": {}})
    );
    server.abort();
}

/// A server going away must be **said out loud**, not waited on forever. Two death modes:
#[tokio::test]
async fn stdio_session_reports_a_server_that_goes_away() {
    // One: it left before we spoke — the write itself finds out.
    let (client_side, server_side) = tokio::io::duplex(1024);
    drop(server_side);
    let (client_read, client_write) = tokio::io::split(client_side);
    let mut session = StdioSession::new(client_read, client_write);
    let error = session.initialize().await.unwrap_err();
    assert!(error.contains("could not write"), "{error}");

    // Two: it left after taking the request — discovered at read time; a 0 read is EOF.
    let (client_side, server_side) = tokio::io::duplex(1024);
    let (mut server_read, server_write) = tokio::io::split(server_side);
    drop(server_write); // it never replies
    let drain = tokio::spawn(async move {
        let mut line = String::new();
        let _ = BufReader::new(&mut server_read).read_line(&mut line).await;
        // Let go right after reading: once the read end closes, the client should see EOF.
    });
    let (client_read, client_write) = tokio::io::split(client_side);
    let mut session = StdioSession::new(client_read, client_write);
    let error = session.initialize().await.unwrap_err();
    assert!(
        error.contains("the server closed the connection"),
        "{error}"
    );
    drain.await.expect("drain that line");
}

/// A scripted local HTTP server: records every request it receives.
struct FakeHttp {
    url: String,
    seen: Arc<Mutex<Vec<(String, String)>>>,
}

struct Reply {
    status: u16,
    content_type: &'static str,
    body: String,
}

async fn fake_http(script: Vec<Reply>) -> FakeHttp {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("the address"));
    let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let index = Arc::new(AtomicUsize::new(0));
    let (script, sink) = (Arc::new(script), seen.clone());
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let (script, index, sink) = (script.clone(), index.clone(), sink.clone());
            tokio::spawn(async move {
                loop {
                    let Some((headers, body)) = read_http_request(&mut stream).await else {
                        return;
                    };
                    sink.lock().await.push((headers, body.clone()));
                    let at = index.fetch_add(1, Ordering::SeqCst);
                    let reply = &script[at.min(script.len() - 1)];
                    // The `@id` in the reply becomes this request's id — the client counts ids
                    // itself; the script must not pretend to know where the count is.
                    let id = serde_json::from_str::<Value>(&body)
                        .ok()
                        .and_then(|message| message.get("id").cloned())
                        .unwrap_or(Value::Null);
                    let payload = reply.body.replace("@id", &id.to_string());
                    let head = format!(
                        "HTTP/1.1 {} X\r\ncontent-type: {}\r\ncontent-length: {}\r\nmcp-session-id: abc\r\n\r\n",
                        reply.status,
                        reply.content_type,
                        payload.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(payload.as_bytes()).await;
                    let _ = stream.flush().await;
                }
            });
        }
    });
    FakeHttp { url, seen }
}

async fn read_http_request<S>(stream: &mut S) -> Option<(String, String)>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut headers = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        headers.push_str(&line);
        if line == "\r\n" || line == "\n" {
            break;
        }
    }
    let length: usize = headers
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    tokio::io::AsyncReadExt::read_exact(&mut reader, &mut body)
        .await
        .ok()?;
    Some((headers, String::from_utf8_lossy(&body).to_string()))
}

#[tokio::test]
async fn http_session_handshakes_sends_the_token_and_reads_sse() {
    let reply = |body: &str| Reply {
        status: 200,
        content_type: "application/json",
        body: body.to_string(),
    };
    let fake = fake_http(vec![
        reply(r#"{"jsonrpc":"2.0","id":@id,"result":{"protocolVersion":"2025-06-18"}}"#),
        Reply {
            status: 202,
            content_type: "application/json",
            body: String::new(),
        },
        Reply {
            status: 200,
            content_type: "text/event-stream",
            body: "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/x\"}\n\n\
                   event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":@id,\"result\":{\"tools\":[{\"name\":\"reverse\"}]}}\n\n"
                .to_string(),
        },
        reply(r#"{"jsonrpc":"2.0","id":@id,"result":{"content":[{"type":"text","text":"cba"}]}}"#),
    ])
    .await;

    let mut session = HttpSession::new(&fake.url, Some("s3cret".to_string()));
    session.initialize().await.expect("handshake");
    let tools = session
        .list_tools()
        .await
        .expect("list the tools (over SSE)");
    assert_eq!(tools[0].name, "reverse");
    let text = session
        .call_tool("reverse", json!({"text": "abc"}))
        .await
        .expect("call it");
    assert_eq!(text, "cba");

    let seen = fake.seen.lock().await;
    assert!(seen[0].0.contains("Bearer s3cret"), "{}", seen[0].0);
    // The initialize answer gave a session id; every later request must carry it back.
    assert!(seen[1].0.contains("mcp-session-id: abc"), "{}", seen[1].0);
    assert!(seen[2].0.contains("mcp-session-id: abc"), "{}", seen[2].0);
    // A notification's answer is 202: not an error, and not a result either.
    assert!(
        seen[1].1.contains("notifications/initialized"),
        "{}",
        seen[1].1
    );
    assert!(seen[2].0.contains("text/event-stream"), "{}", seen[2].0);
}

#[tokio::test]
async fn http_failure_reports_the_status_code() {
    let fake = fake_http(vec![Reply {
        status: 401,
        content_type: "text/html",
        body: "<html><body>unauthorized</body></html>".to_string(),
    }])
    .await;
    let mut session = HttpSession::new(&fake.url, None);
    let error = session.initialize().await.unwrap_err();
    assert!(error.contains("401"), "{error}");
    // A whole page of HTML must not be dumped into the log.
    assert!(error.len() < 200, "{error}");
}

/// Transport failure does not prove that the remote tool did not commit its action.
#[tokio::test]
async fn a_dead_transport_is_rebuilt_without_replaying_an_unknown_outcome() {
    let reply = |body: &str| Reply {
        status: 200,
        content_type: "application/json",
        body: body.to_string(),
    };
    let fake = fake_http(vec![
        Reply {
            status: 500,
            content_type: "text/plain",
            body: "connection lost after commit".into(),
        },
        reply(r#"{"jsonrpc":"2.0","id":@id,"result":{"protocolVersion":"2025-06-18"}}"#),
        Reply {
            status: 202,
            content_type: "application/json",
            body: String::new(),
        },
        reply(
            r#"{"jsonrpc":"2.0","id":@id,"result":{"content":[{"type":"text","text":"alive"}]}}"#,
        ),
    ])
    .await;

    // Stands in for a transport that died: a closed port fails at the first request.
    let dead = crate::transport::Transport::Http(HttpSession::new(&fake.url, None));
    let connection = crate::Connection::with_policy("x", dead, crate::BreakerPolicy::default())
        .with_spec(TransportSpec::Http {
            url: fake.url.clone(),
            token: None,
        });

    let text = connection.call("echo", json!({})).await;
    assert!(text.contains("outcome unknown"), "{text}");
    assert_eq!(
        fake.seen.lock().await.len(),
        3,
        "one tool call plus the reconnect handshake; no replay"
    );
    let next = connection.call("echo", json!({})).await;
    assert_eq!(
        next, "alive",
        "an explicit later call can use the rebuilt connection"
    );
    assert_eq!(fake.seen.lock().await.len(), 4);
}

/// Without a spec there is nothing to rebuild from: say that, do not pretend to retry.
#[tokio::test]
async fn a_transport_without_a_spec_says_it_cannot_be_rebuilt() {
    let dead = crate::transport::Transport::Http(HttpSession::new("http://127.0.0.1:1/mcp", None));
    let connection = crate::Connection::with_policy("x", dead, crate::BreakerPolicy::default());
    let text = connection.call("echo", json!({})).await;
    assert!(text.contains("cannot be rebuilt"), "{text}");
}

// ---- 3. A real subprocess server -------------------------------------------

/// Fake MCP server: hand-written JSON-RPC, no SDK. Deliberately sends a dotted tool name.
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
    method = msg.get("method")
    if "id" not in msg:
        continue
    if method == "initialize":
        result = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                  "serverInfo": {"name": "fake", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [
            {"name": "reverse", "description": "reverse the text",
             "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}},
                             "required": ["text"]}},
            {"name": "dotted.name"},
            {"name": "boom"}]}
    elif method == "tools/call":
        params = msg.get("params") or {}
        name = params.get("name")
        if name == "reverse":
            text = (params.get("arguments") or {}).get("text", "")
            result = {"content": [{"type": "text", "text": text[::-1]}]}
        elif name == "boom":
            result = {"content": [{"type": "text", "text": "blew up"}], "isError": True}
        else:
            result = {"content": [{"type": "text", "text": "ok"}]}
    else:
        send({"jsonrpc": "2.0", "id": msg["id"],
              "error": {"code": -32601, "message": "no such method"}})
        continue
    send({"jsonrpc": "2.0", "id": msg["id"], "result": result})
"#;

fn python3() -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("python3")
        .arg("--version")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| std::path::PathBuf::from("python3"))
}

async fn tool_ctx() -> joyczl_tools::ToolCtx {
    let dir = tempfile::tempdir().expect("temp dir");
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("open the store");
    let _ = dir.keep(); // sqlite still writes -wal/-shm: the dir must not be dropped here
    joyczl_tools::ToolCtx {
        on_uncertain: None,
        resource_lease: None,
        task_run_id: None,
        activate: None,
        embedder: None,
        cwd: None,
        approval: None,
        jobs: None,
        hooks: None,
        turn_limits: std::sync::Arc::new(joyczl_tools::web::TurnLimits::default()),
        session_id: "test".to_string(),
        facts: joyczl_state::Facts::new(pool.clone()),
        episodes: joyczl_state::Episodes::new(pool.clone()),
        chat: joyczl_state::Chat::new(pool.clone()),
        calendar: joyczl_state::Calendar::new(pool),
        home: std::path::PathBuf::from(".joy"),
        on_delta: None,
    }
}

#[tokio::test]
async fn a_real_subprocess_server_registers_working_tools() {
    let Some(python) = python3() else {
        eprintln!("skipped: no python3 on this machine");
        return;
    };
    let dir = tempfile::tempdir().expect("temp dir");
    let script = dir.path().join("fake_mcp_server.py");
    std::fs::write(&script, FAKE_SERVER).expect("write the server");
    let config = dir.path().join("mcp.json");
    std::fs::write(
        &config,
        json!({"servers": [{"name": "demo", "command": python.display().to_string(),
                            "args": [script.display().to_string()]}]})
        .to_string(),
    )
    .expect("write the config");

    let client = McpClient::connect(&config).await;
    assert_eq!(
        client.warnings(),
        Vec::<String>::new(),
        "there should be no warning"
    );
    assert_eq!(client.servers(), vec!["demo"]);

    let mut registry = joyczl_tools::ToolRegistry::new();
    for tool in client.tools() {
        registry.register(tool);
    }
    assert_eq!(
        registry.names(),
        vec!["demo_boom", "demo_dotted_name", "demo_reverse"]
    );

    let reverse = registry.get("demo_reverse").expect("reverse is there");
    assert_eq!(reverse.description, "[MCP:demo] reverse the text");
    assert_eq!(reverse.input_schema["required"], json!(["text"]));
    // The one without a server schema: degrade to an empty object, not a missing field.
    assert_eq!(
        registry
            .get("demo_dotted_name")
            .expect("present")
            .input_schema,
        json!({"type": "object", "properties": {}})
    );

    // End-to-end: the name the model reads is renamed; the name sent back must be the original.
    let ctx = tool_ctx().await;
    assert_eq!(
        registry
            .execute(ctx.clone(), "demo_reverse", json!({"text": "abc"}))
            .await,
        "cba"
    );
    assert_eq!(
        registry
            .execute(ctx.clone(), "demo_dotted_name", json!({}))
            .await,
        "ok"
    );
    // A failed call is also **text**: the model can read it; the loop does not crash.
    let failed = registry.execute(ctx, "demo_boom", json!({})).await;
    assert!(
        failed.starts_with("Error: MCP tool boom failed:"),
        "{failed}"
    );
    assert!(
        !failed.contains("outcome unknown"),
        "a remote tool error is a known outcome: {failed}"
    );
    assert!(failed.contains("blew up"), "{failed}");
}

#[tokio::test]
async fn a_server_that_cannot_start_is_skipped_with_a_warning() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = dir.path().join("mcp.json");
    std::fs::write(
        &config,
        json!({"servers": [{"name": "ghost", "command": "definitely-not-a-command-xyz"}]})
            .to_string(),
    )
    .expect("write the config");

    let client = McpClient::connect(&config).await;
    // One server failing to start affects neither the others nor Joy's startup.
    assert!(client.servers().is_empty());
    assert_eq!(client.warnings().len(), 1, "{:?}", client.warnings());
    assert!(
        client.warnings()[0].contains("ghost"),
        "{:?}",
        client.warnings()
    );
    assert_eq!(
        client.call("ghost", "reverse", json!({})).await,
        "MCP server 'ghost' is not connected."
    );
}

// ---- Breaker and name dedup (third batch) ----------------------------------

/// A repeatedly failing server gets circuit-broken: **no knocking** while open; one probe after cooldown.
#[tokio::test]
async fn a_failing_server_gets_circuit_broken() {
    use crate::{BreakerPolicy, Connection, Transport};
    use std::time::Duration;

    let fake = fake_http(vec![
        // Handshake (the successful one)
        Reply {
            status: 200,
            content_type: "application/json",
            body: r#"{"jsonrpc":"2.0","id":@id,"result":{"protocolVersion":"2025-06-18"}}"#
                .to_string(),
        },
        // Handshake's second leg: the 202 of `notifications/initialized`
        Reply {
            status: 202,
            content_type: "application/json",
            body: String::new(),
        },
        // Then 500 every time (the script's last entry gets reused)
        Reply {
            status: 500,
            content_type: "application/json",
            body: "boom".to_string(),
        },
    ])
    .await;

    let mut session = HttpSession::new(&fake.url, None);
    session.initialize().await.expect("handshake");

    // Threshold 2, cooldown 150ms: tests don't wait 60 seconds for a breaker.
    let connection = Connection::with_policy(
        "flaky",
        Transport::Http(session),
        BreakerPolicy {
            failures: 2,
            cooldown: Duration::from_millis(150),
        },
    );

    let first = connection.call("do", json!({})).await;
    let second = connection.call("do", json!({})).await;
    assert!(first.contains("failed"), "{first}");
    assert!(second.contains("failed"), "{second}");

    // Third call: breaker open — it must not be disturbed again.
    let before = fake.seen.lock().await.len();
    let skipped = connection.call("do", json!({})).await;
    assert!(skipped.contains("circuit breaker"), "{skipped}");
    assert!(skipped.contains("was skipped"), "{skipped}");
    assert_eq!(
        fake.seen.lock().await.len(),
        before,
        "nothing may go out while the breaker is open"
    );

    // Cooled down: let one probe through (half-open).
    tokio::time::sleep(Duration::from_millis(220)).await;
    let probe = connection.call("do", json!({})).await;
    assert!(
        !probe.contains("circuit breaker"),
        "once cooled down one probe has to be let through; got: {probe}"
    );
    assert!(
        probe.contains("failed"),
        "the probe really did knock: {probe}"
    );
}

/// Two servers each reporting a name-colliding tool: nobody overwrites anybody.
#[test]
fn two_servers_that_collide_get_distinct_names() {
    use std::collections::HashSet;

    let mut taken: HashSet<String> = HashSet::new();
    // Both `a` + `b_c` and `a_b` + `c` become `a_b_c`.
    let (first, renamed) = config::model_safe_name_unique("a", "b_c", &mut taken);
    assert_eq!(first, "a_b_c");
    assert!(!renamed);

    let (second, renamed) = config::model_safe_name_unique("a_b", "c", &mut taken);
    assert!(renamed, "a collision has to be renamed");
    assert_ne!(second, first, "the earlier one must not be overwritten");
    assert!(second.starts_with("a_b_c_"), "{second}");
    assert!(
        second.len() <= 64,
        "the 64-character cap still applies: {second}"
    );
}

/// Even with a suffix, a long name must stay within 64 chars.
#[test]
fn a_renamed_long_tool_name_still_fits() {
    use std::collections::HashSet;

    let mut taken: HashSet<String> = HashSet::new();
    let long = "x".repeat(200);
    let (first, _) = config::model_safe_name_unique("s", &long, &mut taken);
    let (second, renamed) = config::model_safe_name_unique("s", &long, &mut taken);
    assert!(renamed);
    assert!(second.len() <= 64, "{second}");
    assert_ne!(first, second);
}
