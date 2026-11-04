//! Hook contracts: loading, matching, blocking, rewriting, the timeout dichotomy, trusted hash.
//!
//! Cases needing real subprocesses are marked `#[cfg(unix)]`: hooks run shell commands, which
//! on Windows are a different beast (see `docs/limitations.md`); don't pretend platforms are alike.

use super::hooks::{HookEvent, Hooks};
use serde_json::json;
// The shell-subprocess cases are unix-only (see the module comment above); the HTTP-handler
// cases are not — a TcpListener is a TcpListener on every platform CI builds on.

fn write_hooks(dir: &std::path::Path, body: &str) {
    std::fs::write(dir.join("hooks.json"), body).expect("写 hooks.json");
}

// ---- Loading (no subprocess needed) -----------------------------------------

#[test]
fn a_missing_file_means_no_hooks() {
    let dir = tempfile::tempdir().expect("临时目录");
    assert!(Hooks::load(dir.path(), None).is_none(), "没配就一次都不起");
}

#[test]
fn a_broken_file_config_does_not_take_the_process_down() {
    let dir = tempfile::tempdir().expect("临时目录");
    write_hooks(dir.path(), "{ 这不是 JSON");
    assert!(Hooks::load(dir.path(), None).is_none(), "解析不了就当没配");
}

#[test]
fn unknown_events_and_entries_without_a_command_are_skipped_with_a_reason() {
    let dir = tempfile::tempdir().expect("临时目录");
    write_hooks(
        dir.path(),
        r#"{ "hooks": {
             "NotAnEvent": [ { "command": "true" } ],
             "PreToolUse": [ { "matcher": "x" }, { "command": "true" } ]
           } }"#,
    );
    let hooks = Hooks::load(dir.path(), None).expect("装载");
    assert_eq!(hooks.count, 1, "只有一条是真能跑的");
    assert!(
        hooks.summary().contains("PreToolUse×1"),
        "{}",
        hooks.summary()
    );
}

#[test]
fn disable_all_hooks_wins_over_everything() {
    let dir = tempfile::tempdir().expect("临时目录");
    write_hooks(
        dir.path(),
        r#"{ "disableAllHooks": true, "hooks": { "PreToolUse": [ {"command": "true"} ] } }"#,
    );
    let hooks = Hooks::load(dir.path(), None).expect("装载");
    assert_eq!(hooks.count, 0);
    assert!(
        hooks.summary().contains("disableAllHooks"),
        "{}",
        hooks.summary()
    );
}

#[test]
fn no_entries_means_no_hooks_at_all() {
    let dir = tempfile::tempdir().expect("临时目录");
    write_hooks(dir.path(), r#"{ "hooks": {} }"#);
    assert!(Hooks::load(dir.path(), None).is_none());
}

// ---- Behavior (subprocesses actually run) -----------------------------------

#[cfg(unix)]
fn hooks_with(dir: &std::path::Path, body: &str) -> Hooks {
    write_hooks(dir, body);
    Hooks::load(dir, None).expect("装载")
}

/// exit 2 is the only block-via-exit-code; the reason comes from stderr.
#[cfg(unix)]
#[tokio::test]
async fn exit_two_blocks_and_carries_the_reason() {
    let dir = tempfile::tempdir().expect("临时目录");
    let hooks = hooks_with(
        dir.path(),
        r#"{ "hooks": { "PreToolUse": [
             { "command": "sh", "args": ["-c", "cat >/dev/null; echo 这条不许跑 >&2; exit 2"] }
           ] } }"#,
    );

    let outcome = hooks
        .fire(HookEvent::PreToolUse, json!({"tool_name": "run_command"}))
        .await;
    assert_eq!(outcome.blocked.as_deref(), Some("这条不许跑"));
}

/// exit 1 is a **non-blocking** error: the action proceeds, but an aside must record it.
#[cfg(unix)]
#[tokio::test]
async fn exit_one_is_an_error_but_not_a_block() {
    let dir = tempfile::tempdir().expect("临时目录");
    let hooks = hooks_with(
        dir.path(),
        r#"{ "hooks": { "PostToolUse": [
             { "command": "sh", "args": ["-c", "cat >/dev/null; echo 我这个钩子坏了 >&2; exit 1"] }
           ] } }"#,
    );

    let outcome = hooks
        .fire(HookEvent::PostToolUse, json!({"tool_name": "save_note"}))
        .await;
    assert!(outcome.blocked.is_none(), "exit 1 不该阻断");
    assert!(
        outcome.note.unwrap_or_default().contains("我这个钩子坏了"),
        "但要说出来"
    );
}

/// Decision JSON: block, rewrite input, rewrite output, add context.
#[cfg(unix)]
#[tokio::test]
async fn a_decision_json_can_block_rewrite_and_add_context() {
    let dir = tempfile::tempdir().expect("临时目录");
    let hooks = hooks_with(
        dir.path(),
        r#"{ "hooks": {
             "PreToolUse": [ { "command": "sh",
               "args": ["-c", "cat >/dev/null; printf %s '{\"updatedInput\":{\"command\":\"ls -la\"}}'"] } ],
             "PostToolUse": [ { "command": "sh",
               "args": ["-c", "cat >/dev/null; printf %s '{\"updatedOutput\":\"我把它改短了\"}'"] } ],
             "SessionStart": [ { "command": "sh",
               "args": ["-c", "cat >/dev/null; printf %s '{\"additionalContext\":\"这个仓库的规矩在 CONTRIBUTING.md\"}'"] } ]
           } }"#,
    );

    let pre = hooks
        .fire(HookEvent::PreToolUse, json!({"tool_name": "run_command"}))
        .await;
    assert_eq!(pre.input.unwrap()["command"], "ls -la");

    let post = hooks
        .fire(HookEvent::PostToolUse, json!({"tool_name": "run_command"}))
        .await;
    assert_eq!(post.output.as_deref(), Some("我把它改短了"));

    let start = hooks
        .fire(HookEvent::SessionStart, json!({"session_id": "s1"}))
        .await;
    assert!(start
        .context
        .unwrap_or_default()
        .contains("CONTRIBUTING.md"));
}

/// `permissionDecision: allow` → someone decided for the user.
#[cfg(unix)]
#[tokio::test]
async fn a_permission_decision_of_allow_counts_as_consent() {
    let dir = tempfile::tempdir().expect("临时目录");
    let hooks = hooks_with(
        dir.path(),
        r#"{ "hooks": { "PermissionRequest": [ { "command": "sh",
             "args": ["-c", "cat >/dev/null; printf %s '{\"hookSpecificOutput\":{\"permissionDecision\":\"allow\"}}'"] } ] } }"#,
    );
    let outcome = hooks
        .fire(
            HookEvent::PermissionRequest,
            json!({"tool_name": "run_command"}),
        )
        .await;
    assert!(outcome.allowed, "该记为已获准");
    assert!(outcome.blocked.is_none());
}

/// The timeout dichotomy: policy events fail-closed, observation events fail-open.
#[cfg(unix)]
#[tokio::test]
async fn a_timeout_blocks_policy_events_but_not_observers() {
    let dir = tempfile::tempdir().expect("临时目录");
    // 1s timeout + sleep 3s: must time out.
    let body = r#"{ "hooks": {
         "PreToolUse": [ { "timeout": 1, "command": "sh", "args": ["-c", "cat >/dev/null; sleep 3"] } ],
         "PostToolUse": [ { "timeout": 1, "command": "sh", "args": ["-c", "cat >/dev/null; sleep 3"] } ]
       } }"#;
    let hooks = hooks_with(dir.path(), body);

    let policy = hooks
        .fire(HookEvent::PreToolUse, json!({"tool_name": "run_command"}))
        .await;
    assert!(
        policy.blocked.is_some(),
        "策略事件没能在时限内表态 = 不放行"
    );

    let observer = hooks
        .fire(HookEvent::PostToolUse, json!({"tool_name": "run_command"}))
        .await;
    assert!(
        observer.blocked.is_none(),
        "观察事件超时不该影响已经发生的事"
    );
}

/// Matchers: `*` hits all, `a|b` any, otherwise exact equality (no regex).
#[cfg(unix)]
#[tokio::test]
async fn matchers_pick_which_calls_the_hook_sees() {
    let dir = tempfile::tempdir().expect("临时目录");
    // On a hit, write plain text to stdout — an observation event's stdout becomes an aside.
    let hooks = hooks_with(
        dir.path(),
        r#"{ "hooks": { "PostToolUse": [
             { "matcher": "save_note|forget_note", "command": "sh", "args": ["-c", "cat >/dev/null; echo 命中了"] }
           ] } }"#,
    );

    let hit = hooks
        .fire(HookEvent::PostToolUse, json!({"tool_name": "forget_note"}))
        .await;
    assert!(hit.note.unwrap_or_default().contains("命中了"));

    let miss = hooks
        .fire(HookEvent::PostToolUse, json!({"tool_name": "run_command"}))
        .await;
    assert!(miss.note.is_none(), "没匹配就不该跑");
}

/// hooks.json changed at runtime: **don't run** and say why (aligned with trusted_hash).
#[cfg(unix)]
#[tokio::test]
async fn a_file_changed_while_running_is_not_executed() {
    let dir = tempfile::tempdir().expect("临时目录");
    let marker = dir.path().join("跑过了");
    let hooks = hooks_with(
        dir.path(),
        &format!(
            r#"{{ "hooks": {{ "PostToolUse": [
                 {{ "command": "sh", "args": ["-c", "cat >/dev/null; touch {}"] }}
               ] }} }}"#,
            marker.display()
        ),
    );

    // Change the file (simulating another process writing it).
    write_hooks(
        dir.path(),
        r#"{ "hooks": { "PostToolUse": [ { "command": "sh", "args": ["-c", "cat >/dev/null; echo 新内容"] } ] } }"#,
    );

    let outcome = hooks
        .fire(HookEvent::PostToolUse, json!({"tool_name": "save_note"}))
        .await;
    assert!(!marker.exists(), "改过之后不该再执行");
    assert!(
        outcome.note.unwrap_or_default().contains("改过"),
        "要说明为什么没执行"
    );
}

// ---- HTTP handler: same payload, same decision contract, same failure dichotomy ----

/// A minimal HTTP endpoint that serves exactly one request: reads the request head plus the
/// Content-Length body, replies `body_json` (HTTP 200). Returns the endpoint address and the
/// server task. Test-only — the same idea as provider_tests' Scripted, but one round trip.
fn spawn_http_once(body_json: &'static str) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑端口");
    listener.set_nonblocking(true).expect("设非阻塞");
    let listener = tokio::net::TcpListener::from_std(listener).expect("转 tokio 监听器");
    let addr = listener.local_addr().expect("取地址");
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("接受连接");
        let mut buf = Vec::new();
        // Read until the head terminator (\r\n\r\n).
        loop {
            let mut chunk = [0u8; 1024];
            let n = socket.read(&mut chunk).await.expect("读请求");
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&buf);
        let len: usize = head
            .split("\r\n")
            .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let have = buf.len() - buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() - 4;
        let mut body = buf.split_off(buf.len() - have);
        while body.len() < len {
            let mut chunk = [0u8; 1024];
            let n = socket.read(&mut chunk).await.expect("读 body");
            body.extend_from_slice(&chunk[..n]);
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body_json.len(),
            body_json
        );
        socket.write_all(response.as_bytes()).await.expect("写应答");
        socket.flush().await.expect("刷缓冲");
    });
    (addr, handle)
}

fn hooks_with_http(event: &str, url: &str) -> Hooks {
    let dir = tempfile::tempdir().expect("临时目录");
    let body = json!({ "hooks": { event: [{ "url": url }] } }).to_string();
    write_hooks(dir.path(), &body);
    // The directory must outlive the fire(): the trust hash re-reads the file before every run —
    // a missing file reads as "changed" and refuses to run. keep() takes ownership (giving up
    // auto-cleanup) in exchange for a stable path.
    let path = dir.keep();
    Hooks::load(&path, None).expect("装载")
}

#[tokio::test]
async fn http_handler_receives_the_payload_and_can_block() {
    let (addr, server) = spawn_http_once(r#"{"decision":"block","reason":"策略说不行"}"#);
    let hooks = hooks_with_http("PreToolUse", &format!("http://{addr}/hook"));
    let outcome = hooks
        .fire(
            HookEvent::PreToolUse,
            json!({ "tool_name": "run_command", "tool_input": { "command": "ls" } }),
        )
        .await;
    assert_eq!(
        outcome.blocked.as_deref(),
        Some("策略说不行"),
        "HTTP 应答走同一决策契约"
    );
    server.await.expect("服务端任务");
}

#[tokio::test]
async fn http_handler_can_feed_additional_context() {
    let (addr, server) = spawn_http_once(r#"{"additionalContext":"这是 hook 的话"}"#);
    let hooks = hooks_with_http("SessionStart", &format!("http://{addr}/"));
    let outcome = hooks
        .fire(HookEvent::SessionStart, json!({ "session_id": "s" }))
        .await;
    assert_eq!(outcome.context.as_deref(), Some("这是 hook 的话"));
    server.await.expect("服务端任务");
}

#[tokio::test]
async fn http_connection_failure_fails_open_with_a_stderr_line() {
    // Bind a port nobody listens on: the connection is refused. Same rule as the shell's
    // "won't start" — fail-open plus a stderr line.
    let hooks = hooks_with_http("PostToolUse", "http://127.0.0.1:1/");
    let outcome = hooks
        .fire(
            HookEvent::PostToolUse,
            json!({ "tool_name": "run_command" }),
        )
        .await;
    assert!(outcome.blocked.is_none(), "连接失败不是拒绝");
    assert!(outcome.note.is_some(), "但要留痕");
}

#[test]
fn an_entry_with_both_command_and_url_is_skipped_at_load() {
    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::write(
        dir.path().join("hooks.json"),
        json!({ "hooks": { "Stop": [
            { "command": "sh", "args": ["-c", "exit 0"], "url": "http://127.0.0.1:1/" }
        ] } })
        .to_string(),
    )
    .expect("写配置");
    assert!(
        Hooks::load(dir.path(), None).is_none(),
        "两条 handler 都写 = 配置错误，整条跳过后零条目 → None"
    );
}
