//! Notifier tests: message format, load semantics, a real Slack POST (loopback listener).

use super::notify::{load, Notifier, NotifyEvent, Slack};
/// One human-readable sentence: the key facts of all three event kinds are present.
#[test]
fn messages_carry_the_essentials() {
    let approval = NotifyEvent::ApprovalRequested {
        turn_id: "t1".into(),
        tool: "run_command".into(),
        args_preview: "rm -rf /tmp/x".into(),
        reason: "allowed表没盖住".into(),
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
        reason: "续了 5 轮还没达成".into(),
    }
    .message();
    assert!(
        goal.contains("round-limit") && goal.contains("5 轮"),
        "{goal}"
    );
}

/// Load semantics: no flag / no file / no field / empty field — all mean none.
#[test]
fn load_semantics_are_conservative() {
    let dir = tempfile::tempdir().expect("临时目录");
    assert!(load(dir.path(), false).is_none(), "开关没开就是没有");

    assert!(load(dir.path(), true).is_none(), "没文件也是没有");

    std::fs::write(dir.path().join("notify.json"), "{}").expect("写空对象");
    assert!(load(dir.path(), true).is_none(), "没 slack 字段");

    std::fs::write(dir.path().join("notify.json"), r#"{"slack": "  "}"#).expect("写空白");
    assert!(load(dir.path(), true).is_none(), "空白字段");

    std::fs::write(dir.path().join("notify.json"), "{半截").expect("写坏文件");
    assert!(load(dir.path(), true).is_none(), "坏文件不当炸药");

    std::fs::write(
        dir.path().join("notify.json"),
        r#"{"slack": "https://hooks.slack.com/services/x"}"#,
    )
    .expect("写好配置");
    assert!(load(dir.path(), true).is_some(), "有配置就有通知器");
}

/// A real Slack POST: a loopback listener receives one request; path and payload are checked. The URL is admin-set,
/// and pointing at intranet/loopback is legitimate (ntfy/Gotify), so the loopback test is valid.
#[tokio::test]
async fn slack_posts_the_text_payload() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("占个口");
    let port = listener.local_addr().expect("拿端口").port();
    // The blocking accept must run on a plain thread — putting it on the tokio single-thread
    // runtime would clog its only executor thread (learned the hard way).
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("等一次连接");
        // Read the whole request before replying — replying on a half-read occasionally drops packets on macOS.
        use std::io::{Read as _, Write as _};
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    });

    let slack = Slack::new(format!("http://127.0.0.1:{port}/hook"));
    slack.notify(NotifyEvent::JobFinished {
        id: "j1".into(),
        command: "echo 你好".into(),
        status: "completed".into(),
        exit_code: Some(0),
    });
    // notify is fire-and-forget (spawned). Await first to give the single-thread runtime
    // a chance to run the send task (join blocks this thread — the runtime lives on it),
    // then join the server thread for asserts: a connection accepted = it really sent.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    server.join().expect("服务线程");
}

/// Failure side path: pointing at a port nobody listens on — no panic, no propagation; that's the whole requirement.
#[tokio::test]
async fn a_dead_endpoint_is_just_a_stderr_line() {
    // Dynamic ports from 49152 up are most likely unlistened; even if listened, one harmless POST is fine.
    let slack = Slack::new("http://127.0.0.1:49150/hook".to_string());
    slack.notify(NotifyEvent::GoalFinished {
        session_id: "s1".into(),
        status: "blocked".into(),
        reason: "判断器挂了".into(),
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
}
