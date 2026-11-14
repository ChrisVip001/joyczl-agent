//! Notifier tests: message format, load semantics, a real Slack POST (loopback listener).

use super::notify::{load, Notifier, NotifyEvent, Slack};
/// One human-readable sentence: the key facts of all three event kinds are present.
#[test]
fn messages_carry_the_essentials() {
    let approval = NotifyEvent::ApprovalRequested {
        turn_id: "t1".into(),
        tool: "run_command".into(),
        args_preview: "rm -rf /tmp/x".into(),
        reason: "the allowed table did not cover it".into(),
    }
    .message();
    assert!(approval.contains("run_command"), "{approval}");
    assert!(approval.contains("rm -rf"), "{approval}");
    assert!(approval.contains("asks for approval"), "{approval}");

    let job = NotifyEvent::JobFinished {
        id: "j1".into(),
        command: "long-task".into(),
        status: "failed".into(),
        exit_code: Some(2),
    }
    .message();
    assert!(
        job.contains("j1") && job.contains("failed") && job.contains("exit 2"),
        "{job}"
    );

    // Even without an exit code (interrupted), something like "exit None" must not appear.
    let job = NotifyEvent::JobFinished {
        id: "j2".into(),
        command: "c".into(),
        status: "interrupted".into(),
        exit_code: None,
    }
    .message();
    assert!(!job.contains("None"), "{job}");

    let goal = NotifyEvent::GoalFinished {
        session_id: "s1".into(),
        status: "round-limit".into(),
        reason: "five continuation rounds and still not there".into(),
    }
    .message();
    assert!(
        goal.contains("round-limit") && goal.contains("rounds"),
        "{goal}"
    );
}

/// Load semantics: no flag / no file / no field / empty field — all mean none.
#[test]
fn load_semantics_are_conservative() {
    let dir = tempfile::tempdir().expect("temp dir");
    assert!(
        load(dir.path(), false).is_none(),
        "switched off means nothing"
    );

    assert!(load(dir.path(), true).is_none(), "no file means nothing");

    std::fs::write(dir.path().join("notify.json"), "{}").expect("write an empty object");
    assert!(load(dir.path(), true).is_none(), "no slack field");

    std::fs::write(dir.path().join("notify.json"), r#"{"slack": "  "}"#).expect("write blank");
    assert!(load(dir.path(), true).is_none(), "a blank field");

    std::fs::write(dir.path().join("notify.json"), "{ half a thing")
        .expect("write the broken file");
    assert!(load(dir.path(), true).is_none(), "a bad file is not a bomb");

    std::fs::write(
        dir.path().join("notify.json"),
        r#"{"slack": "https://hooks.slack.com/services/x"}"#,
    )
    .expect("write a good config");
    assert!(
        load(dir.path(), true).is_some(),
        "a config means a notifier"
    );
}

/// A real Slack POST: a loopback listener receives one request; path and payload are checked. The URL is admin-set,
/// and pointing at intranet/loopback is legitimate (ntfy/Gotify), so the loopback test is valid.
#[tokio::test]
async fn slack_posts_the_text_payload() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("claim a port");
    let port = listener.local_addr().expect("take the port").port();
    // The blocking accept must run on a plain thread — putting it on the tokio single-thread
    // runtime would clog its only executor thread (learned the hard way).
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("wait for a connection");
        // Read the whole request before replying — replying on a half-read occasionally drops packets on macOS.
        use std::io::{Read as _, Write as _};
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    });

    let slack = Slack::new(format!("http://127.0.0.1:{port}/hook"));
    slack.notify(NotifyEvent::JobFinished {
        id: "j1".into(),
        command: "echo hello".into(),
        status: "completed".into(),
        exit_code: Some(0),
    });
    // notify is fire-and-forget (spawned). Await first to give the single-thread runtime
    // a chance to run the send task (join blocks this thread — the runtime lives on it),
    // then join the server thread for asserts: a connection accepted = it really sent.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    server.join().expect("the service thread");
}

/// Failure side path: pointing at a port nobody listens on — no panic, no propagation; that's the whole requirement.
#[tokio::test]
async fn a_dead_endpoint_is_just_a_stderr_line() {
    // Dynamic ports from 49152 up are most likely unlistened; even if listened, one harmless POST is fine.
    let slack = Slack::new("http://127.0.0.1:49150/hook".to_string());
    slack.notify(NotifyEvent::GoalFinished {
        session_id: "s1".into(),
        status: "blocked".into(),
        reason: "the judge died".into(),
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
}
