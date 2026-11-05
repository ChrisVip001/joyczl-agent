//! Long-task entity tests — one question per test, all through the real dispatch path:
//! does the task row tell the truth about what the human asked for?
//!
//! The interesting half is the pause/resume pair: pausing must **keep** the checkpoint
//! (a pause is not a stop), and continuing must resume it instead of replaying tools.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use joyczl_config::Settings;
use joyczl_protocol::{
    codes, JsonRpcMessage, JsonRpcRequest, RequestId, ServerNotification, TaskEnvelope,
    TurnStartParams,
};
use joyczl_provider::mock::Mock;
use joyczl_provider::{Message, Resolved, Role};
use serde_json::json;
use tokio::sync::mpsc;

use crate::{EventSink, Frame, Server};

/// A server plus its home — booting a second one on the same home is how "restart" is tested.
async fn boot(mock: Option<Arc<Mock>>) -> (Server, PathBuf) {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    let _ = dir.keep();
    let server = Server::boot(
        pool,
        Settings {
            home: home.clone(),
            // Graph routing would spend extra model calls on triage; these tests count
            // turns, not routes — the direct path is the one under test.
            graph_workflows: false,
            ..Settings::default()
        },
    )
    .await;
    if let Some(mock) = mock {
        server.set_resolved(Some(Resolved {
            context_window: joyczl_provider::DEFAULT_CONTEXT_WINDOW,
            vision: false,
            provider_id: "mock".to_string(),
            client: mock,
            model: "test-model".to_string(),
            small_model: "test-small".to_string(),
        }));
    }
    (server, home)
}

/// Send one request and hand back the decoded result (or the error message).
async fn call(
    server: &Server,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let request = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: RequestId::Number(1),
        method: method.to_string(),
        params: Some(params),
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    crate::handle(server, request, EventSink::new(tx)).await;
    let mut response = None;
    while let Ok(frame) = rx.try_recv() {
        if let Frame::Response(message) = frame {
            response = Some(message);
        }
    }
    match response.expect("每个请求必有应答") {
        JsonRpcMessage::Response(response) => Ok(response.result),
        JsonRpcMessage::Error(error) => Err(error.error.message),
    }
}

async fn create_task(
    server: &Server,
    session: &str,
    title: &str,
    goal: Option<&str>,
) -> TaskEnvelope {
    let result = call(
        server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({ "sessionId": session, "title": title, "goalCondition": goal }),
    )
    .await
    .expect("建任务该成功");
    serde_json::from_value(result).expect("TaskEnvelope")
}

#[tokio::test]
async fn a_second_active_task_is_refused_with_the_first_one_named() {
    let (server, _home) = boot(None).await;
    create_task(&server, "default", "盯三天服务器", Some("三天都盯着")).await;

    let error = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({ "sessionId": "default", "title": "另一件事" }),
    )
    .await
    .expect_err("一会话只许一个活跃任务");
    assert!(error.contains("盯三天服务器"), "报错要说清是哪个：{error}");

    // A different session is a different task — the constraint is per session.
    create_task(&server, "other", "另一件事", None).await;
}

/// A task may declare its tier ("the task picks the model"). The declaration lands on the row —
/// every round runs on it — and a typo is refused at creation, not discovered on a vendor bill.
#[tokio::test]
async fn a_task_can_declare_its_tier_and_a_typo_is_refused() {
    let (server, _home) = boot(None).await;

    let error = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({ "sessionId": "default", "title": "读一遍", "tier": "reasoining" }),
    )
    .await
    .expect_err("拼错的档位要当场拒绝");
    assert!(
        error.contains("reasoining"),
        "错误要带上收到的那串：{error}"
    );

    let created = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({ "sessionId": "default", "title": "读一遍", "tier": "reasoning" }),
    )
    .await
    .expect("合法档位该建得成");
    assert_eq!(created["task"]["tier"], json!("reasoning"));

    let id = created["task"]["id"].as_str().expect("有 id");
    let row = server.tasks.get(id).await.expect("查").expect("任务在");
    assert_eq!(
        row.tier.as_deref(),
        Some("reasoning"),
        "档位落在任务行上，不是某一轮"
    );

    // No declaration = no field on the wire; never a made-up "chat".
    let (plain_server, _plain_home) = boot(None).await;
    let plain = create_task(&plain_server, "default", "随手一问", None).await;
    assert!(plain.task.tier.is_none());
}

#[tokio::test]
async fn goal_set_creates_the_task_and_clearing_keeps_the_row() {
    let (server, _home) = boot(None).await;
    let response = call(
        &server,
        joyczl_protocol::methods::GOAL_SET,
        json!({ "sessionId": "default", "condition": "把测试跑绿" }),
    )
    .await
    .expect("设目标该成功");
    assert_eq!(response["active"], json!(true));
    assert_eq!(response["condition"], json!("把测试跑绿"));

    // The compatibility layer: the goal **is** a task row now.
    let task = server
        .tasks
        .active("default")
        .await
        .expect("查")
        .expect("有任务");
    assert_eq!(task.goal_condition.as_deref(), Some("把测试跑绿"));
    assert_eq!(task.title, "把测试跑绿");

    // Clearing the goal downgrades the task to a single-turn one — the row stays as record.
    call(
        &server,
        joyczl_protocol::methods::GOAL_SET,
        json!({ "sessionId": "default" }),
    )
    .await
    .expect("清目标该成功");
    let task = server.tasks.get(&task.id).await.expect("查").expect("还在");
    assert!(task.goal_condition.is_none(), "目标清掉了");
    assert_eq!(task.status, "pending", "任务本身留着");
}

#[tokio::test]
async fn a_single_turn_task_closes_done_when_its_turn_ends() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text("办好了"),
        Mock::text("办好了"),
        Mock::text("办好了"),
    ]));
    let (server, _home) = boot(Some(mock)).await;
    let task = create_task(&server, "default", "查一下日志", None).await;
    assert_eq!(task.task.status, "pending");

    // One ordinary turn in that session — it carries the task, and closes it honestly.
    let (tx, _rx) = mpsc::unbounded_channel();
    crate::turn::run_turn(
        &server,
        TurnStartParams {
            attachments: None,
            session_id: Some("default".to_string()),
            message: "开始吧".to_string(),
            stream: Some(true),
            cwd: None,
            tier: None,
        },
        RequestId::Number(1),
        &EventSink::new(tx),
    )
    .await
    .expect("回合该跑完");

    let row = server
        .tasks
        .get(&task.task.id)
        .await
        .expect("查")
        .expect("还在");
    assert_eq!(row.status, "done", "单回合任务：轮结束即完成");
    assert!(row.finished_at.is_some());
    assert!(row.current_turn_id.is_none(), "心跳清掉了");
}

#[tokio::test]
async fn suspending_keeps_the_checkpoint_and_resuming_continues_it() {
    let mock = Arc::new(Mock::new(vec![Mock::text("接着干完了")]));
    let (server, _home) = boot(Some(mock)).await;
    let task = create_task(&server, "default", "盯三天服务器", Some("盯满三天")).await;

    // Plant the paused turn exactly as a real pause leaves it: a checkpoint with working memory.
    let messages = vec![
        Message::user_text("帮我盯三天服务器"),
        Message {
            role: Role::Assistant,
            content: vec![joyczl_provider::ContentBlock::Text {
                text: "先记一笔".to_string(),
            }],
        },
    ];
    server
        .checkpoints
        .save(
            "t-pause",
            "default",
            "帮我盯三天服务器",
            &serde_json::to_string(&messages).expect("序列化"),
            1,
            "{}",
        )
        .await
        .expect("写 checkpoint");
    server
        .tasks
        .set_running(&task.task.id, "t-pause")
        .await
        .expect("标 running");
    // What a real pause leaves behind: the turn's finalize marks the checkpoint `suspended`
    // (the pause flag was set before the interrupt). Simulate that half honestly.
    server
        .checkpoints
        .finish("t-pause", "suspended")
        .await
        .expect("模拟 finalize 的暂停收尾");

    let suspended = call(
        &server,
        joyczl_protocol::methods::TASK_SUSPEND,
        json!({ "taskId": task.task.id }),
    )
    .await
    .expect("暂停该成功");
    assert_eq!(suspended["task"]["status"], json!("suspended"));
    assert!(
        suspended["task"]["suspendReason"]
            .as_str()
            .is_some_and(|r| !r.is_empty()),
        "暂停理由要记下来"
    );
    let checkpoint = server
        .checkpoints
        .get("t-pause")
        .await
        .expect("查")
        .expect("checkpoint 还在");
    assert_eq!(
        checkpoint.status, "suspended",
        "暂停把 checkpoint 留着，不标 abandoned"
    );
    assert_eq!(
        server
            .tasks
            .get(&task.task.id)
            .await
            .expect("查")
            .expect("在")
            .current_turn_id
            .as_deref(),
        Some("t-pause"),
        "挂起的任务记得自己停在哪个回合"
    );

    let resumed = call(
        &server,
        joyczl_protocol::methods::TASK_RESUME,
        json!({ "taskId": task.task.id }),
    )
    .await
    .expect("继续该成功");
    assert_eq!(resumed["task"]["status"], json!("running"));

    // The resume runs as its own task. Wait for the **reply**, not the checkpoint: finalize marks
    // the checkpoint `done` before it writes the chat row, so waiting on the checkpoint and then
    // reading history is a race (green alone, red under a loaded parallel test run).
    let mut replied = false;
    for _ in 0..200 {
        replied = server
            .chat
            .session_history("default")
            .await
            .expect("读对话日志")
            .iter()
            .any(|(_, content)| content.contains("接着干完了"));
        if replied {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(replied, "续跑的回复落了账");

    let checkpoint = server
        .checkpoints
        .get("t-pause")
        .await
        .expect("查")
        .expect("在");
    assert_eq!(checkpoint.status, "done", "续跑走完了");
}

#[tokio::test]
async fn cancelling_closes_the_task_and_never_auto_continues() {
    let (server, _home) = boot(None).await;
    let task = create_task(&server, "default", "停掉的事", Some("做不完")).await;
    server
        .checkpoints
        .save("t-stop", "default", "停掉的事", "[]", 1, "{}")
        .await
        .expect("写 checkpoint");
    server
        .tasks
        .set_running(&task.task.id, "t-stop")
        .await
        .expect("标 running");

    let cancelled = call(
        &server,
        joyczl_protocol::methods::TASK_CANCEL,
        json!({ "taskId": task.task.id }),
    )
    .await
    .expect("取消该成功");
    assert_eq!(cancelled["task"]["status"], json!("cancelled"));

    let checkpoint = server
        .checkpoints
        .get("t-stop")
        .await
        .expect("查")
        .expect("在");
    assert_eq!(checkpoint.status, "abandoned", "取消的回合绝不自动续跑");
    assert!(
        server.checkpoints.open_rows().await.expect("读").is_empty(),
        "没有可续跑的残留"
    );
    assert!(
        server.tasks.active("default").await.expect("查").is_none(),
        "会话腾出来了，可以开新任务"
    );
}

#[tokio::test]
async fn a_restart_marks_a_running_task_suspended_honestly() {
    let (server, home) = boot(None).await;
    let task = create_task(&server, "default", "跑长任务", Some("做完为止")).await;
    server
        .tasks
        .set_running(&task.task.id, "t-live")
        .await
        .expect("标 running");
    drop(server); // the process "dies" with the task running

    // Same home, fresh process: boot reconcile must not pretend it is still running.
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("重开库");
    let rebooted = Server::boot(
        pool,
        Settings {
            home: home.clone(),
            graph_workflows: false,
            ..Settings::default()
        },
    )
    .await;
    let row = rebooted
        .tasks
        .get(&task.task.id)
        .await
        .expect("查")
        .expect("还在");
    assert_eq!(row.status, "suspended");
    assert!(
        row.suspend_reason
            .as_deref()
            .is_some_and(|r| r.contains("进程重启")),
        "理由得说清楚：{:?}",
        row.suspend_reason
    );
}

#[tokio::test]
async fn an_unknown_task_id_is_an_error_not_a_panic() {
    let (server, _home) = boot(None).await;
    let error = call(
        &server,
        joyczl_protocol::methods::TASK_RESUME,
        json!({ "taskId": "task-nope" }),
    )
    .await
    .expect_err("不存在的任务要报错");
    assert!(error.contains("不存在"), "{error}");

    // And a task that already finished cannot be resumed either.
    let task = create_task(&server, "default", "已完成", None).await;
    server
        .tasks
        .finish(&task.task.id, "done")
        .await
        .expect("收尾");
    let error = call(
        &server,
        joyczl_protocol::methods::TASK_RESUME,
        json!({ "taskId": task.task.id }),
    )
    .await
    .expect_err("完成的任务不能继续");
    assert_eq!(codes::INVALID_PARAMS, -32602);
    assert!(error.contains("挂起"), "{error}");
}

/// The task pings on the event bus carry the state, so another device's card can update
/// without guessing (kind is `task`, the app-server's `TaskUpdated` notification is the source).
#[tokio::test]
async fn task_transitions_are_announced() {
    let (server, _home) = boot(None).await;
    let task = create_task(&server, "default", "要广播的任务", None).await;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let sink = EventSink::new(tx);
    let params = joyczl_protocol::TaskActionParams {
        task_id: task.task.id.clone(),
        reason: Some("测试暂停".to_string()),
    };
    crate::task::suspend(&server, &sink, params)
        .await
        .expect("暂停");

    let mut announced = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        if let Frame::Notification(ServerNotification::TaskUpdated(updated)) = frame {
            announced.push(updated);
        }
    }
    assert!(!announced.is_empty(), "状态变化必须广播，不能静默");
    assert_eq!(announced.last().expect("有").status, "suspended");
    assert_eq!(announced.last().expect("有").task_id, task.task.id);
}
