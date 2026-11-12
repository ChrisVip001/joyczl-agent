//! The full approval chain: run all the way to "the command executed inside the sandbox".
//!
//! This deliberately uses a **real Server + real sandbox** (on machines that have one): the value of the
//! approval mechanism lives entirely in the causality "not before being asked, only after being answered" — fake either half and the test cannot detect it.

use std::sync::Arc;

use joyczl_protocol::{RememberScope, RequestId, ServerNotification, TurnStartParams};
use joyczl_provider::mock::Mock;
use joyczl_provider::Resolved;
use joyczl_tools::approval::RememberTarget;
use tokio::sync::oneshot;

use crate::{run_turn, EventSink, Frame, Server};

/// An unregistered request number → an honest false ("nobody heard your answer"), not an error.
#[tokio::test]
async fn answering_an_unknown_request_is_false_not_an_error() {
    let dir = tempfile::tempdir().expect("临时目录");
    let settings = joyczl_config::Settings {
        home: dir.path().to_path_buf(),
        ..Default::default()
    };
    let server = crate::open(&settings).await.expect("装配");
    assert!(!server.answer_approval("no-such-turn", "ap-1", true, RememberScope::Once));
}

/// In a turn, `run_command` needs approval: the notification goes out and the command really runs only after being answered.
#[tokio::test]
async fn a_turn_asks_and_runs_the_command_only_after_being_answered() {
    if !joyczl_tools::exec::sandbox_available() {
        return; // Consistent with the other sandbox tests: skip on machines without a sandbox (CI)
    }

    let dir = tempfile::tempdir().expect("临时目录");
    let settings = joyczl_config::Settings {
        home: dir.path().to_path_buf(),
        provider: "openai".to_string(),
        api_key: Some("test-key".to_string()),
        model: Some("test-model".to_string()),
        small_model: Some("test-model".to_string()),
        // Exec enabled, but the allow table does **not** match echo — so it must take the approval path.
        exec_enabled: true,
        exec_allow: vec!["ls *".to_string()],
        approval: "on-request".to_string(),
        approval_timeout_secs: 20,
        ..Default::default()
    };
    let server = crate::open(&settings).await.expect("装配");
    server.install_provider(Resolved {
        context_window: joyczl_provider::DEFAULT_CONTEXT_WINDOW,
        vision: false,
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

    // Play the terminal/cockpit: answer "approve" as soon as an approval request appears. That is what the REPL does.
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
                        let accepted = server.answer_approval(
                            &ask.turn_id,
                            &ask.request_id,
                            true,
                            RememberScope::Once,
                        );
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
            attachments: None,
            session_id: Some("test".to_string()),
            message: "跑一下 echo".to_string(),
            stream: Some(true),
            cwd: None,
            tier: None,
        },
        RequestId::Number(1),
        &sink,
    )
    .await
    .expect("这一轮该跑完");

    drop(sink); // Close the channel so the answerer task can finish
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

/// `JOY_APPROVAL=never` (the default): nobody is asked, the command is refused outright — the old behavior unchanged.
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
        context_window: joyczl_provider::DEFAULT_CONTEXT_WINDOW,
        vision: false,
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
            attachments: None,
            session_id: Some("test".to_string()),
            message: "跑一下".to_string(),
            stream: Some(true),
            cwd: None,
            tier: None,
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

// ---- The three remember scopes --------------------------------------------------------------
//
// These go in through the waiting table instead of a whole turn: the question "how long does a
// yes last" is answered by `answer_approval` alone, and a real sandbox would only add noise.

/// Register a question as if a tool had just asked it.
fn ask(
    server: &Server,
    turn_id: &str,
    request_id: &str,
    target: RememberTarget,
) -> oneshot::Receiver<bool> {
    let (tx, rx) = oneshot::channel();
    server
        .approvals
        .lock()
        .expect("approvals 锁不该中毒")
        .entry(turn_id.to_string())
        .or_default()
        .insert(
            request_id.to_string(),
            crate::approval::Waiting {
                tx,
                remember: Some(target),
            },
        );
    rx
}

fn settings_json(home: &std::path::Path) -> String {
    std::fs::read_to_string(joyczl_config::patch_path(home)).unwrap_or_default()
}

/// "记住这条" (command): written to `settings.json` **and** into the live cell, so the next call
/// sees it — the old "takes effect after a restart" is gone.
#[tokio::test]
async fn remembering_a_command_persists_and_takes_effect_at_once() {
    let dir = tempfile::tempdir().expect("临时目录");
    let settings = joyczl_config::Settings {
        home: dir.path().to_path_buf(),
        ..Default::default()
    };
    let server = crate::open(&settings).await.expect("装配");

    let rx = ask(
        &server,
        "t1",
        "ap-1",
        RememberTarget::Command("git status".to_string()),
    );
    assert!(server.answer_approval("t1", "ap-1", true, RememberScope::Always));
    assert!(rx.await.expect("答案要送达"), "批准");

    assert!(
        server
            .exec_session_allow
            .lock()
            .expect("锁")
            .contains(&"git status".to_string()),
        "本进程立刻生效"
    );
    assert!(
        server
            .settings()
            .exec_allow
            .iter()
            .any(|rule| rule == "git status"),
        "也要进内存里的设置"
    );
    let raw = settings_json(dir.path());
    assert!(raw.contains("git status"), "而且写进 settings.json：{raw}");
}

/// `once` leaves no trace; `session` stays in memory and never reaches the disk.
#[tokio::test]
async fn once_and_session_leave_different_traces() {
    let dir = tempfile::tempdir().expect("临时目录");
    let settings = joyczl_config::Settings {
        home: dir.path().to_path_buf(),
        ..Default::default()
    };
    let server = crate::open(&settings).await.expect("装配");

    let rx = ask(
        &server,
        "t1",
        "ap-1",
        RememberTarget::Command("make".to_string()),
    );
    assert!(server.answer_approval("t1", "ap-1", true, RememberScope::Once));
    assert!(rx.await.expect("送达"));
    assert!(
        !server
            .exec_session_allow
            .lock()
            .expect("锁")
            .contains(&"make".to_string()),
        "只这次 = 什么都不记"
    );
    assert_eq!(settings_json(dir.path()), "", "一次性的批准不写盘");

    let rx = ask(
        &server,
        "t2",
        "ap-2",
        RememberTarget::Command("cargo test".to_string()),
    );
    assert!(server.answer_approval("t2", "ap-2", true, RememberScope::Session));
    assert!(rx.await.expect("送达"));
    assert!(
        server
            .exec_session_allow
            .lock()
            .expect("锁")
            .contains(&"cargo test".to_string()),
        "本会话记着"
    );
    assert_eq!(settings_json(dir.path()), "", "会话作用域不该写盘");
}

/// A rejected answer remembers nothing — a "no" must never be turned into a standing rule.
#[tokio::test]
async fn a_denial_is_never_remembered() {
    let dir = tempfile::tempdir().expect("临时目录");
    let settings = joyczl_config::Settings {
        home: dir.path().to_path_buf(),
        ..Default::default()
    };
    let server = crate::open(&settings).await.expect("装配");
    let rx = ask(
        &server,
        "t1",
        "ap-1",
        RememberTarget::Command("rm x".to_string()),
    );
    assert!(server.answer_approval("t1", "ap-1", false, RememberScope::Always));
    assert!(!rx.await.expect("送达"), "拒绝就是拒绝");
    assert!(server.exec_session_allow.lock().expect("锁").is_empty());
    assert_eq!(settings_json(dir.path()), "");
}

/// A remembered write **directory** makes the next write land with no broker at all.
/// The naming difference matters: the human approved the folder, not the file name.
#[tokio::test]
async fn a_remembered_write_directory_needs_no_answer() {
    let dir = tempfile::tempdir().expect("临时目录");
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("建工作目录");
    let settings = joyczl_config::Settings {
        home: dir.path().join("home"),
        write_enabled: true,
        approval: "on-request".to_string(),
        ..Default::default()
    };
    let server = crate::open(&settings).await.expect("装配");

    let rx = ask(
        &server,
        "t1",
        "ap-1",
        RememberTarget::Directory(workspace.clone()),
    );
    assert!(server.answer_approval("t1", "ap-1", true, RememberScope::Always));
    assert!(rx.await.expect("送达"));

    // No broker in this ctx: only the remembered directory can let the write through.
    let ctx = server.tool_ctx("s1", Some(workspace.clone()), None);
    let path = workspace.join("note.md");
    let out = server
        .tools
        .execute(
            ctx,
            "write_file",
            serde_json::json!({"path": path.display().to_string(), "content": "hi\n"}),
        )
        .await;
    assert!(out.starts_with("已写入"), "{out}");
    assert!(path.exists(), "真写了");
    assert!(
        settings_json(&settings.home).contains(workspace.to_string_lossy().as_ref()),
        "目录进了 settings.json"
    );
}
