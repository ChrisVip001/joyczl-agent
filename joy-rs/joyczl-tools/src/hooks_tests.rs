//! Hook contracts: loading, matching, blocking, rewriting, the timeout dichotomy, trusted hash.
//!
//! Cases needing real subprocesses are marked `#[cfg(unix)]`: hooks run shell commands, which
//! on Windows are a different beast (see `docs/limitations.md`); don't pretend platforms are alike.

use super::hooks::{HookEvent, Hooks};
use serde_json::json;
// The shell-subprocess cases are unix-only (see the module comment above); the HTTP-handler
// cases are not — a TcpListener is a TcpListener on every platform CI builds on.

fn write_hooks(dir: &std::path::Path, body: &str) {
    std::fs::write(dir.join("hooks.json"), body).expect("write hooks.json");
}

// ---- Loading (no subprocess needed) -----------------------------------------

#[test]
fn a_missing_file_means_no_hooks() {
    let dir = tempfile::tempdir().expect("temp dir");
    assert!(
        Hooks::load(dir.path(), None).is_none(),
        "unconfigured means nothing ever runs"
    );
}

#[test]
fn a_broken_file_config_does_not_take_the_process_down() {
    let dir = tempfile::tempdir().expect("temp dir");
    write_hooks(dir.path(), "{ this is not JSON");
    assert!(
        Hooks::load(dir.path(), None).is_none(),
        "unparseable is treated as unconfigured"
    );
}

#[test]
fn unknown_events_and_entries_without_a_command_are_skipped_with_a_reason() {
    let dir = tempfile::tempdir().expect("temp dir");
    write_hooks(
        dir.path(),
        r#"{ "hooks": {
             "NotAnEvent": [ { "command": "true" } ],
             "PreToolUse": [ { "matcher": "x" }, { "command": "true" } ]
           } }"#,
    );
    let hooks = Hooks::load(dir.path(), None).expect("load");
    assert_eq!(hooks.count, 1, "only one of them can actually run");
    assert!(
        hooks.summary().contains("PreToolUse×1"),
        "{}",
        hooks.summary()
    );
}

#[test]
fn disable_all_hooks_wins_over_everything() {
    let dir = tempfile::tempdir().expect("temp dir");
    write_hooks(
        dir.path(),
        r#"{ "disableAllHooks": true, "hooks": { "PreToolUse": [ {"command": "true"} ] } }"#,
    );
    let hooks = Hooks::load(dir.path(), None).expect("load");
    assert_eq!(hooks.count, 0);
    assert!(
        hooks.summary().contains("disableAllHooks"),
        "{}",
        hooks.summary()
    );
}

#[test]
fn no_entries_means_no_hooks_at_all() {
    let dir = tempfile::tempdir().expect("temp dir");
    write_hooks(dir.path(), r#"{ "hooks": {} }"#);
    assert!(Hooks::load(dir.path(), None).is_none());
}

// ---- Behavior (subprocesses actually run) -----------------------------------

#[cfg(unix)]
fn hooks_with(dir: &std::path::Path, body: &str) -> Hooks {
    write_hooks(dir, body);
    Hooks::load(dir, None).expect("load")
}

/// exit 2 is the only block-via-exit-code; the reason comes from stderr.
#[cfg(unix)]
#[tokio::test]
async fn exit_two_blocks_and_carries_the_reason() {
    let dir = tempfile::tempdir().expect("temp dir");
    let hooks = hooks_with(
        dir.path(),
        r#"{ "hooks": { "PreToolUse": [
             { "command": "sh", "args": ["-c", "cat >/dev/null; echo this one must not run >&2; exit 2"] }
           ] } }"#,
    );

    let outcome = hooks
        .fire(HookEvent::PreToolUse, json!({"tool_name": "run_command"}))
        .await;
    assert_eq!(
        outcome.blocked.as_deref(),
        Some("this one must not run"),
        "the hook's reason is passed through"
    );
}

/// exit 1 is a **non-blocking** error: the action proceeds, but an aside must record it.
#[cfg(unix)]
#[tokio::test]
async fn exit_one_is_an_error_but_not_a_block() {
    let dir = tempfile::tempdir().expect("temp dir");
    let hooks = hooks_with(
        dir.path(),
        r#"{ "hooks": { "PostToolUse": [
             { "command": "sh", "args": ["-c", "cat >/dev/null; echo my hook is broken >&2; exit 1"] }
           ] } }"#,
    );

    let outcome = hooks
        .fire(HookEvent::PostToolUse, json!({"tool_name": "save_note"}))
        .await;
    assert!(outcome.blocked.is_none(), "exit 1 must not block");
    assert!(
        outcome
            .note
            .unwrap_or_default()
            .contains("my hook is broken"),
        "but it has to say so"
    );
}

/// Decision JSON: block, rewrite input, rewrite output, add context.
#[cfg(unix)]
#[tokio::test]
async fn a_decision_json_can_block_rewrite_and_add_context() {
    let dir = tempfile::tempdir().expect("temp dir");
    let hooks = hooks_with(
        dir.path(),
        r#"{ "hooks": {
             "PreToolUse": [ { "command": "sh",
               "args": ["-c", "cat >/dev/null; printf %s '{\"updatedInput\":{\"command\":\"ls -la\"}}'"] } ],
             "PostToolUse": [ { "command": "sh",
               "args": ["-c", "cat >/dev/null; printf %s '{\"updatedOutput\":\"I shortened it\"}'"] } ],
             "SessionStart": [ { "command": "sh",
               "args": ["-c", "cat >/dev/null; printf %s '{\"additionalContext\":\"this repo keeps its rules in CONTRIBUTING.md\"}'"] } ]
           } }"#,
    );

    let pre = hooks
        .fire(HookEvent::PreToolUse, json!({"tool_name": "run_command"}))
        .await;
    assert_eq!(pre.input.unwrap()["command"], "ls -la");

    let post = hooks
        .fire(HookEvent::PostToolUse, json!({"tool_name": "run_command"}))
        .await;
    assert_eq!(post.output.as_deref(), Some("I shortened it"));

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
    let dir = tempfile::tempdir().expect("temp dir");
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
    assert!(outcome.allowed, "it should be recorded as approved");
    assert!(outcome.blocked.is_none());
}

/// The timeout dichotomy: policy events fail-closed, observation events fail-open.
#[cfg(unix)]
#[tokio::test]
async fn a_timeout_blocks_policy_events_but_not_observers() {
    let dir = tempfile::tempdir().expect("temp dir");
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
        "a policy event that cannot decide in time counts as not allowed"
    );

    let observer = hooks
        .fire(HookEvent::PostToolUse, json!({"tool_name": "run_command"}))
        .await;
    assert!(
        observer.blocked.is_none(),
        "a timed-out observation event must not affect what already happened"
    );
}

/// Matchers: `*` hits all, `a|b` any, otherwise exact equality (no regex).
#[cfg(unix)]
#[tokio::test]
async fn matchers_pick_which_calls_the_hook_sees() {
    let dir = tempfile::tempdir().expect("temp dir");
    // On a hit, write plain text to stdout — an observation event's stdout becomes an aside.
    let hooks = hooks_with(
        dir.path(),
        r#"{ "hooks": { "PostToolUse": [
             { "matcher": "save_note|forget_note", "command": "sh", "args": ["-c", "cat >/dev/null; echo matched"] }
           ] } }"#,
    );

    let hit = hooks
        .fire(HookEvent::PostToolUse, json!({"tool_name": "forget_note"}))
        .await;
    assert!(hit.note.unwrap_or_default().contains("matched"));

    let miss = hooks
        .fire(HookEvent::PostToolUse, json!({"tool_name": "run_command"}))
        .await;
    assert!(miss.note.is_none(), "no match means it does not run");
}

/// hooks.json changed at runtime: **don't run** and say why (aligned with trusted_hash).
#[cfg(unix)]
#[tokio::test]
async fn a_file_changed_while_running_is_not_executed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let marker = dir.path().join("it ran");
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
        r#"{ "hooks": { "PostToolUse": [ { "command": "sh", "args": ["-c", "cat >/dev/null; echo new content"] } ] } }"#,
    );

    let outcome = hooks
        .fire(HookEvent::PostToolUse, json!({"tool_name": "save_note"}))
        .await;
    assert!(
        !marker.exists(),
        "after `was changed` it must not run again"
    );
    assert!(
        outcome.note.unwrap_or_default().contains("was changed"),
        "it has to say why it did not run"
    );
}

// ---- HTTP handler: same payload, same decision contract, same failure dichotomy ----

/// A minimal HTTP endpoint that serves exactly one request: reads the request head plus the
/// Content-Length body, replies `body_json` (HTTP 200). Returns the endpoint address and the
/// server task. Test-only — the same idea as provider_tests' Scripted, but one round trip.
fn spawn_http_once(body_json: &'static str) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the port");
    listener.set_nonblocking(true).expect("set it non-blocking");
    let listener = tokio::net::TcpListener::from_std(listener).expect("hand it to tokio");
    let addr = listener.local_addr().expect("take the address");
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept the connection");
        let mut buf = Vec::new();
        // Read until the head terminator (\r\n\r\n).
        loop {
            let mut chunk = [0u8; 1024];
            let n = socket.read(&mut chunk).await.expect("read the request");
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
            let n = socket.read(&mut chunk).await.expect("read the body");
            body.extend_from_slice(&chunk[..n]);
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body_json.len(),
            body_json
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write the response");
        socket.flush().await.expect("flush");
    });
    (addr, handle)
}

fn hooks_with_http(event: &str, url: &str) -> Hooks {
    let dir = tempfile::tempdir().expect("temp dir");
    let body = json!({ "hooks": { event: [{ "url": url }] } }).to_string();
    write_hooks(dir.path(), &body);
    // The directory must outlive the fire(): the trust hash re-reads the file before every run —
    // a missing file reads as "changed" and refuses to run. keep() takes ownership (giving up
    // auto-cleanup) in exchange for a stable path.
    let path = dir.keep();
    Hooks::load(&path, None).expect("load")
}

#[tokio::test]
async fn http_handler_receives_the_payload_and_can_block() {
    let (addr, server) = spawn_http_once(r#"{"decision":"block","reason":"policy said no"}"#);
    let hooks = hooks_with_http("PreToolUse", &format!("http://{addr}/hook"));
    let outcome = hooks
        .fire(
            HookEvent::PreToolUse,
            json!({ "tool_name": "run_command", "tool_input": { "command": "ls" } }),
        )
        .await;
    assert_eq!(
        outcome.blocked.as_deref(),
        Some("policy said no"),
        "an HTTP response goes through the same decision contract"
    );
    server.await.expect("the server task");
}

#[tokio::test]
async fn http_handler_can_feed_additional_context() {
    let (addr, server) = spawn_http_once(r#"{"additionalContext":"this is the hook's word"}"#);
    let hooks = hooks_with_http("SessionStart", &format!("http://{addr}/"));
    let outcome = hooks
        .fire(HookEvent::SessionStart, json!({ "session_id": "s" }))
        .await;
    assert_eq!(
        outcome.context.as_deref(),
        Some("this is the hook's word"),
        "the hook's note is passed through"
    );
    server.await.expect("the server task");
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
    assert!(
        outcome.blocked.is_none(),
        "a failed connection is not a refusal"
    );
    assert!(outcome.note.is_some(), "but it has to leave a trace");
}

#[test]
fn an_entry_with_both_command_and_url_is_skipped_at_load() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("hooks.json"),
        json!({ "hooks": { "Stop": [
            { "command": "sh", "args": ["-c", "exit 0"], "url": "http://127.0.0.1:1/" }
        ] } })
        .to_string(),
    )
    .expect("write the config");
    assert!(
        Hooks::load(dir.path(), None).is_none(),
        "both handlers written = a config error; the whole rule is skipped, zero entries → None"
    );
}
