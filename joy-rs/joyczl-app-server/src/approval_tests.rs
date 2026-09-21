//! 批准的全链路：一路真跑到「命令在沙箱里执行了」。
//!
//! 这里刻意用**真 Server + 真沙箱**（有沙箱的机器上）：批准机制的价值全在
//! 「问之前不跑、答了之后才跑」这一条因果上，假掉哪一半都测不出来。

use std::sync::Arc;

use joyczl_protocol::{RequestId, ServerNotification, TurnStartParams};
use joyczl_provider::mock::Mock;
use joyczl_provider::Resolved;

use crate::{run_turn, EventSink, Frame, Server};

/// 没登记过的请求号 → 如实回 false（「你的回答没人听」），不是报错。
#[tokio::test]
async fn answering_an_unknown_request_is_false_not_an_error() {
    let dir = tempfile::tempdir().expect("临时目录");
    let settings = joyczl_config::Settings {
        home: dir.path().to_path_buf(),
        ..Default::default()
    };
    let server = crate::open(&settings).await.expect("装配");
    assert!(!server.answer_approval("no-such-turn", "ap-1", true, false));
}

/// 一轮对话里 `run_command` 需要批准：通知出去、被回答之后命令才真的跑。
#[tokio::test]
async fn a_turn_asks_and_runs_the_command_only_after_being_answered() {
    if !joyczl_tools::exec::sandbox_available() {
        return; // 与其它沙箱测试一致：没沙箱的机器（CI）跳过
    }

    let dir = tempfile::tempdir().expect("临时目录");
    let settings = joyczl_config::Settings {
        home: dir.path().to_path_buf(),
        provider: "openai".to_string(),
        api_key: Some("test-key".to_string()),
        model: Some("test-model".to_string()),
        small_model: Some("test-model".to_string()),
        // 开执行工具，但放行表**不匹配** echo —— 于是它必须走批准那条路。
        exec_enabled: true,
        exec_allow: vec!["ls *".to_string()],
        approval: "on-request".to_string(),
        approval_timeout_secs: 20,
        ..Default::default()
    };
    let server = crate::open(&settings).await.expect("装配");
    server.install_provider(Resolved {
        provider_id: "mock".to_string(),
        client: Arc::new(Mock::new(vec![
            Mock::text(r#"{"retrieve": false, "query": "", "reason": "tool"}"#),
            Mock::tool_use(
                "call-1",
                "run_command",
                serde_json::json!({"command": "echo approved-ok"}),
            ),
            Mock::text("跑完了。"),
        ])),
        model: "test-model".to_string(),
        small_model: "test-model".to_string(),
    });

    let (sink, mut rx) = EventSink::channel();

    // 扮演终端/驾驶舱：见到批准请求就回答「同意」。这就是 REPL 做的事。
    let answerer = tokio::spawn({
        let server = server.clone();
        async move {
            let mut asked = None;
            let mut tool_output = None;
            while let Some(frame) = rx.recv().await {
                let Frame::Notification(notification) = frame else {
                    continue;
                };
                match notification {
                    ServerNotification::ApprovalRequested(ask) => {
                        let accepted =
                            server.answer_approval(&ask.turn_id, &ask.request_id, true, false);
                        asked = Some((ask.args_preview.clone(), accepted));
                    }
                    ServerNotification::ToolCompleted(done) => {
                        tool_output = Some(done.output.clone());
                    }
                    _ => {}
                }
            }
            (asked, tool_output)
        }
    });

    run_turn(
        &server,
        TurnStartParams {
            session_id: Some("test".to_string()),
            message: "跑一下 echo".to_string(),
            stream: Some(true),
        },
        RequestId::Number(1),
        &sink,
    )
    .await
    .expect("这一轮该跑完");

    drop(sink); // 关掉通道，让回答任务收工
    let (asked, tool_output) = answerer.await.expect("回答任务");
    let (preview, accepted) = asked.expect("该有批准请求");
    assert_eq!(preview, "echo approved-ok", "问的必须是那条命令本身");
    assert!(accepted, "回答在时限内送达");

    let output = tool_output.expect("该有工具完成通知");
    assert!(
        output.contains("approved-ok"),
        "批准之后命令要真的在沙箱里跑起来：{output}"
    );
    assert!(output.contains("退出码 0"), "{output}");
}

/// `JOY_APPROVAL=never`（默认）：没人问，命令直接拒 —— 从前的行为不变。
#[tokio::test]
async fn without_on_request_the_command_is_simply_refused() {
    if !joyczl_tools::exec::sandbox_available() {
        return;
    }
    let dir = tempfile::tempdir().expect("临时目录");
    let settings = joyczl_config::Settings {
        home: dir.path().to_path_buf(),
        provider: "openai".to_string(),
        api_key: Some("test-key".to_string()),
        model: Some("test-model".to_string()),
        small_model: Some("test-model".to_string()),
        exec_enabled: true,
        exec_allow: vec!["ls *".to_string()],
        ..Default::default()
    };
    let server: Server = crate::open(&settings).await.expect("装配");
    server.install_provider(Resolved {
        provider_id: "mock".to_string(),
        client: Arc::new(Mock::new(vec![
            Mock::text(r#"{"retrieve": false, "query": "", "reason": "tool"}"#),
            Mock::tool_use(
                "call-1",
                "run_command",
                serde_json::json!({"command": "echo nope"}),
            ),
            Mock::text("好，不跑。"),
        ])),
        model: "test-model".to_string(),
        small_model: "test-model".to_string(),
    });

    let (sink, mut rx) = EventSink::channel();
    let watcher = tokio::spawn(async move {
        let mut asked = false;
        let mut output = String::new();
        while let Some(frame) = rx.recv().await {
            if let Frame::Notification(notification) = frame {
                match notification {
                    ServerNotification::ApprovalRequested(_) => asked = true,
                    ServerNotification::ToolCompleted(done) => output = done.output.clone(),
                    _ => {}
                }
            }
        }
        (asked, output)
    });

    run_turn(
        &server,
        TurnStartParams {
            session_id: Some("test".to_string()),
            message: "跑一下".to_string(),
            stream: Some(true),
        },
        RequestId::Number(1),
        &sink,
    )
    .await
    .expect("这一轮该跑完");

    drop(sink);
    let (asked, output) = watcher.await.expect("观察任务");
    assert!(!asked, "never 模式不该问任何问题");
    assert!(output.starts_with("Error:"), "{output}");
    assert!(output.contains("放行"), "拒因要指向放行表：{output}");
}
