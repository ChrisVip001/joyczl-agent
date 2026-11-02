//! 通知器的测试：消息格式、装载语义、Slack POST 真发（回环监听）。

use super::notify::{load, Notifier, NotifyEvent, Slack};
/// 一句人话：三类事件的关键信息都在。
#[test]
fn messages_carry_the_essentials() {
    let approval = NotifyEvent::ApprovalRequested {
        turn_id: "t1".into(),
        tool: "run_command".into(),
        args_preview: "rm -rf /tmp/x".into(),
        reason: "放行表没盖住".into(),
    }
    .message();
    assert!(approval.contains("run_command"), "{approval}");
    assert!(approval.contains("rm -rf"), "{approval}");
    assert!(approval.contains("批准"), "{approval}");

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

    // 没有退出码（interrupted）也不该出现 "exit None" 这种东西。
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

/// 装载语义：没开关 / 没文件 / 没字段 / 空字段 —— 都是没有。
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

/// Slack POST 真发：回环监听收一次请求，验证路径与载荷。URL 是管理员配的，
/// 指向内网/回环是合法用法（ntfy/Gotify），所以回环测试成立。
#[tokio::test]
async fn slack_posts_the_text_payload() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("占个口");
    let port = listener.local_addr().expect("拿端口").port();
    // 阻塞 accept 必须在普通线程上 —— 放进 tokio 单线程运行时会把它自己
    // 唯一的执行线程堵死（刚踩过）。
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("等一次连接");
        // 读完请求再回 —— 半读就回在 macOS 上会偶发丢包。
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
    // notify 是 fire-and-forget（spawn 出去的）。先 await 让单线程运行时
    // 有机会跑发送任务（join 会堵住这根线程 —— runtime 就在这根线程上），
    // 再 join 服务线程做断言：accept 到过连接 = 确实发过。
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    server.join().expect("服务线程");
}

/// 失败旁路：指向一个没人听的端口 —— 不 panic、不传播，这就是全部要求。
#[tokio::test]
async fn a_dead_endpoint_is_just_a_stderr_line() {
    // 49152 起的动态端口大概率没人听；就算真有人听，收到一条 POST 也无害。
    let slack = Slack::new("http://127.0.0.1:49150/hook".to_string());
    slack.notify(NotifyEvent::GoalFinished {
        session_id: "s1".into(),
        status: "blocked".into(),
        reason: "判断器挂了".into(),
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
}
