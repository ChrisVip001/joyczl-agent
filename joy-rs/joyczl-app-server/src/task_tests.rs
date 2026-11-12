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
    boot_with(mock, false).await
}

/// Same boot, with the planner switch (`JOY_PLAN`) set explicitly — planning is opt-in.
async fn boot_with(mock: Option<Arc<Mock>>, plan_enabled: bool) -> (Server, PathBuf) {
    let dir = tempfile::tempdir().expect("temporary directory");
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
            // Acceptance conditions are about files that really exist, so the write tools must
            // be registered: without them the "fake completion" tests would pass for the wrong
            // reason (no tool at all, instead of a tool that did not write).
            write_enabled: true,
            plan_enabled,
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
async fn several_tasks_may_be_open_in_one_session_and_the_newest_is_the_card() {
    let (server, _home) = boot(None).await;
    let first = create_task(&server, "default", "盯三天服务器", Some("三天都盯着")).await;

    // 0.14 removed "one active task per session": two intents can genuinely coexist, and the
    // row used to be the only place a task could live.
    let second = create_task(&server, "default", "另一件事", None).await;
    assert_ne!(first.task.id, second.task.id);

    let open = server.tasks.list_active().await.expect("查活跃任务");
    assert_eq!(open.len(), 2, "两个都还在");

    // The card shows one: the newest. `task/list` shows both.
    let view = call(
        &server,
        joyczl_protocol::methods::TASK_VIEW,
        json!({ "sessionId": "default" }),
    )
    .await
    .expect("看板该成功");
    assert_eq!(view["task"]["title"], json!("另一件事"), "卡片给最新那个");

    // A different session is a different scope, as always.
    create_task(&server, "other", "第三件事", None).await;
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
async fn setting_a_goal_creates_the_task_and_clearing_keeps_the_row() {
    let (server, _home) = boot(None).await;
    let row = server
        .set_task_goal("default", Some("把测试跑绿"))
        .await
        .expect("设目标该建任务");

    // There is no separate goal record any more: the condition is a column on this row.
    assert_eq!(row.goal_condition.as_deref(), Some("把测试跑绿"));
    assert_eq!(row.title, "把测试跑绿");

    // Clearing the goal downgrades the task to a single-turn one — the row stays as record.
    server
        .set_task_goal("default", None)
        .await
        .expect("清目标该成功");
    let task = server.tasks.get(&row.id).await.expect("查").expect("还在");
    assert!(task.goal_condition.is_none(), "目标清掉了");
    assert_eq!(
        task.status,
        joyczl_state::TaskStatus::Pending,
        "任务本身留着"
    );
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
    assert_eq!(
        row.status,
        joyczl_state::TaskStatus::Done,
        "单回合任务：轮结束即完成"
    );
    assert!(row.finished_at.is_some());
    assert!(row.current_turn_id.is_none(), "心跳清掉了");
}

#[tokio::test]
async fn suspending_keeps_the_checkpoint_and_resuming_continues_it() {
    let mock = Arc::new(Mock::new(vec![Mock::text("接着干完了")]));
    let (server, _home) = boot(Some(mock)).await;
    let task = create_task(&server, "default", "盯三天服务器", Some("盯满三天")).await;

    server
        .runs
        .start("t-pause", &task.task.id, Some("default"), Some("t-pause"))
        .await
        .expect("run");
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
            &serde_json::to_string(&messages).expect("serialize"),
            1,
            "{}",
            Some("t-pause"),
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
        .save("t-stop", "default", "停掉的事", "[]", 1, "{}", None)
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
    assert_eq!(row.status, joyczl_state::TaskStatus::Suspended);
    assert!(
        row.suspend_reason
            .as_deref()
            .is_some_and(|r| r.contains("the process restarted")),
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
    assert!(error.contains("no such task"), "{error}");

    // And a task that already finished cannot be resumed either.
    let task = create_task(&server, "default", "已完成", None).await;
    server
        .tasks
        .finish(&task.task.id, joyczl_state::TaskStatus::Done)
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
    assert!(error.contains("suspended"), "{error}");
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

// ---- 0.14：任务独立存在 + 完成由事实决定 ------------------------------------------------

/// Drive one turn in `session`, with an optional working directory.
async fn drive(server: &Server, session: &str, message: &str, cwd: Option<&str>) {
    let (tx, _rx) = mpsc::unbounded_channel();
    crate::turn::run_turn(
        server,
        TurnStartParams {
            attachments: None,
            session_id: Some(session.to_string()),
            message: message.to_string(),
            stream: Some(true),
            cwd: cwd.map(str::to_string),
            tier: None,
        },
        RequestId::Number(1),
        &EventSink::new(tx),
    )
    .await
    .expect("回合该跑完");
}

/// A task no longer needs a conversation: "do this thing" is a complete sentence.
#[tokio::test]
async fn a_task_can_exist_without_any_session() {
    let (server, _home) = boot(None).await;
    let created = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({ "title": "整理下载目录", "acceptance": [
            { "kind": "fileExists", "path": "index.md" }
        ] }),
    )
    .await
    .expect("无会话也该建得成");

    assert!(created["task"]["sessionId"].is_null(), "没有会话就是没有");
    let id = created["task"]["id"].as_str().expect("有 id");
    assert_eq!(created["task"]["status"], json!("pending"));
    // The condition is normalised at creation, so the card and the checker always agree.
    assert_eq!(
        created["task"]["acceptance"][0]["label"],
        json!("index.md exists")
    );
    assert_eq!(created["task"]["acceptance"][0]["min"], json!(1));

    // The only way in for such a task: by id.
    let got = call(
        &server,
        joyczl_protocol::methods::TASK_GET,
        json!({ "taskId": id }),
    )
    .await
    .expect("按 id 该查得到");
    assert_eq!(got["task"]["title"], json!("整理下载目录"));

    let listed = call(&server, joyczl_protocol::methods::TASK_LIST, json!({}))
        .await
        .expect("列表该成功");
    assert_eq!(listed["tasks"].as_array().expect("数组").len(), 1);
}

/// The model says "done", the file is not there, the rounds are gone: **needs a human**.
/// Not `done` (the lie would stick), not `failed` (the work may still be finishable).
#[tokio::test]
async fn a_task_whose_acceptance_fails_needs_a_human_not_a_done() {
    // One response per model call: the retrieval gate speaks first (fail-open on plain text),
    // then the turn's own reply.
    let mock = Arc::new(Mock::new(vec![
        Mock::text("skip"),
        Mock::text("报告已经写好了，请查收。"),
    ]));
    let (server, _home) = boot(Some(mock)).await;
    let dir = tempfile::tempdir().expect("temporary directory");
    let workspace = dir.path().display().to_string();

    let created = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({
            "sessionId": "default",
            "title": "写一份报告",
            "maxRounds": 1,
            "cwd": workspace,
            "acceptance": [{ "kind": "fileExists", "path": "report.md" }],
        }),
    )
    .await
    .expect("建任务");
    let id = created["task"]["id"].as_str().expect("有 id").to_string();

    drive(&server, "default", "开始吧", None).await;

    let row = server.tasks.get(&id).await.expect("查").expect("还在");
    assert_eq!(
        row.status,
        joyczl_state::TaskStatus::NeedsAttention,
        "验收没过就不能算完成：{:?}",
        row.last_stop_reason
    );
    let reason = row.last_stop_reason.as_deref().expect("要说清为什么停");
    assert!(reason.contains("report.md does not exist"), "{reason}");

    // The card shows the measurement per condition, not a vague "failed".
    let checks: serde_json::Value = serde_json::from_str(&row.checks_json).expect("验收结果");
    assert_eq!(checks[0]["passed"], json!(false));
    assert_eq!(checks[0]["label"], json!("report.md exists"));
}

/// The positive twin: a real write makes the same condition pass — and the artifact list
/// records the file that is actually on disk.
#[tokio::test]
async fn a_condition_passes_only_after_the_file_really_exists() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text("skip"),
        Mock::tool_use(
            "call_1",
            "write_file",
            json!({ "path": "report.md", "content": "结论：可以上线\n" }),
        ),
        Mock::text("写好了。"),
    ]));
    let (server, _home) = boot(Some(mock)).await;
    let dir = tempfile::tempdir().expect("temporary directory");
    let workspace = dir.path().display().to_string();

    let created = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({
            "sessionId": "default",
            "title": "写一份报告",
            "cwd": workspace,
            "acceptance": [
                { "kind": "fileExists", "path": "report.md" },
                { "kind": "fileContains", "path": "report.md", "pattern": "结论", "min": 1 }
            ],
        }),
    )
    .await
    .expect("建任务");
    let id = created["task"]["id"].as_str().expect("有 id").to_string();

    drive(&server, "default", "开始吧", None).await;

    let row = server.tasks.get(&id).await.expect("查").expect("还在");
    assert_eq!(
        row.status,
        joyczl_state::TaskStatus::Done,
        "文件真的在，该完成：{:?}",
        row.last_stop_reason
    );
    assert!(row.finished_at.is_some());

    // Deliverables are files that exist, with their real size.
    let artifacts = server.artifacts.list(&id, 10).await.expect("查交付物");
    assert_eq!(artifacts.len(), 1);
    assert!(artifacts[0].path.ends_with("report.md"));
    assert!(artifacts[0].bytes > 0);

    // One tool call = one step, keyed so a re-observation cannot duplicate it.
    let runs = server.runs.list_by_task(&id, 10).await.expect("查执行");
    assert_eq!(runs.len(), 1, "一次回合就是一次执行");
    assert_eq!(runs[0].status, "completed");
    assert!(runs[0].tools_ok >= 1, "成功调用记在账上");

    let steps = server.steps.list_by_run(&runs[0].id).await.expect("查步骤");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].tool.as_deref(), Some("write_file"));
    assert_eq!(steps[0].status, "ok");
    let key = steps[0].idempotency_key.as_deref().expect("有幂等键");
    assert!(key.ends_with(":1"), "键要带步号：{key}");
    assert!(key.starts_with(&runs[0].id), "键要带回合：{key}");
}

/// A cap that is reached stops the task **before** another round can spend: the state is
/// `needs_attention` (a human may raise the cap), and the reason names the dimension.
#[tokio::test]
async fn a_task_over_budget_stops_before_it_spends_more() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text("skip"),
        Mock::text("先做一点。"),
        Mock::text("再做一点。"),
    ]));
    let (server, _home) = boot(Some(mock)).await;

    let created = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({
            "sessionId": "default",
            "title": "慢慢来",
            "budget": { "tokens": 10 },
        }),
    )
    .await
    .expect("建任务");
    let id = created["task"]["id"].as_str().expect("有 id").to_string();

    drive(&server, "default", "开始吧", None).await;

    let row = server.tasks.get(&id).await.expect("查").expect("还在");
    assert_eq!(row.status, joyczl_state::TaskStatus::NeedsAttention);
    assert!(row.spent_tokens >= 10, "花费要记上：{}", row.spent_tokens);
    let reason = row.last_stop_reason.as_deref().expect("要说清是哪一条超了");
    assert!(
        reason.contains("Over budget") && reason.contains("tokens"),
        "{reason}"
    );
}

/// Every turn that carries the task is its own execution: the card's "执行 N 次" is a count of
/// rows, not of rounds.
#[tokio::test]
async fn each_turn_that_carries_the_task_is_its_own_run() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text("skip"),
        Mock::text("第一趟。"),
        Mock::text("skip"),
        Mock::text("第二趟。"),
    ]));
    let (server, _home) = boot(Some(mock)).await;
    let dir = tempfile::tempdir().expect("temporary directory");
    let workspace = dir.path().display().to_string();

    // `needs_attention` is open on purpose: a human may look at the failed condition and just
    // ask again — that is what makes a second execution possible at all.
    let created = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({
            "sessionId": "default",
            "title": "分几次做",
            "maxRounds": 1,
            "cwd": workspace,
            "acceptance": [{ "kind": "fileExists", "path": "report.md" }],
        }),
    )
    .await
    .expect("建任务");
    let id = created["task"]["id"].as_str().expect("有 id").to_string();

    drive(&server, "default", "第一步", None).await;
    assert_eq!(
        server.runs.count_by_task(&id).await.expect("查执行次数"),
        1,
        "第一趟一次执行"
    );
    let row = server.tasks.get(&id).await.expect("查").expect("还在");
    assert_eq!(row.status, joyczl_state::TaskStatus::NeedsAttention);
    assert!(row.current_turn_id.is_none(), "心跳要清掉");

    // The second turn picks the same task up again — a new run row, same task.
    drive(&server, "default", "第二步", None).await;
    assert_eq!(
        server.runs.count_by_task(&id).await.expect("查执行次数"),
        2,
        "第二趟是新的执行"
    );

    // Both attempts are readable, newest first.
    let runs = server.runs.list_by_task(&id, 10).await.expect("查执行");
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].status, "waiting", "停在验收上，等人确认");
    assert!(runs[0].stop_reason.is_some());
}

/// Rounds inside one turn share a step numbering, not a step **key**: `<turn_id>:<seq>` has to
/// stay unique, or round 2's tool call collides with round 1's row and is silently dropped —
/// which reads as "the tool never ran" and leaves `toolsOk` counting one success forever.
#[tokio::test]
async fn each_round_keeps_its_own_step_so_tools_ok_counts_them_all() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text("skip"),
        Mock::tool_use(
            "call_1",
            "write_file",
            json!({ "path": "a.md", "content": "A\n" }),
        ),
        Mock::text("第一份。"),
        Mock::text("skip"),
        Mock::tool_use(
            "call_2",
            "write_file",
            json!({ "path": "b.md", "content": "B\n" }),
        ),
        Mock::text("两批都好了。"),
    ]));
    let (server, _home) = boot(Some(mock)).await;
    let dir = tempfile::tempdir().expect("temporary directory");
    let workspace = dir.path().display().to_string();

    let created = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({
            "sessionId": "default",
            "title": "分两轮落两份文件",
            "cwd": workspace,
            "acceptance": [{ "kind": "toolsOk", "tool": "write_file", "min": 2 }],
        }),
    )
    .await
    .expect("建任务");
    let id = created["task"]["id"].as_str().expect("有 id").to_string();

    drive(&server, "default", "开始吧", None).await;

    let row = server.tasks.get(&id).await.expect("查").expect("还在");
    assert_eq!(
        row.status,
        joyczl_state::TaskStatus::Done,
        "两次真的写成功就该完成：{:?}",
        row.last_stop_reason
    );
    assert_eq!(row.rounds_used, 2, "成绩来自两轮，不是一轮");

    let runs = server.runs.list_by_task(&id, 10).await.expect("查执行");
    let steps = server.steps.list_by_run(&runs[0].id).await.expect("查步骤");
    assert_eq!(steps.len(), 2, "两轮各一条，谁也没被幂等键吞掉");
    let mut keys: Vec<&str> = steps
        .iter()
        .filter_map(|step| step.idempotency_key.as_deref())
        .collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), 2, "幂等键必须互不相同：{keys:?}");
}

/// A stop is the human's: interrupting one round must end the task loop **right there**.
///
/// Before this, the interrupted round's acceptance failed and "keep trying" bought another
/// round — the user's stop only took effect once the round cap ran out. The case file gives the
/// mock exactly two replies: if a second round were started, the turn would fail on an empty
/// queue instead of coming back suspended.
#[tokio::test]
async fn an_interrupted_round_suspends_the_task_instead_of_spending_another() {
    let mock = Arc::new(Mock::streaming(vec![
        Mock::text("skip"),
        Mock::text("我先把思路理清楚，然后就开始动手写这份报告，还有很多话没有说完。"),
    ]));
    let (server, _home) = boot(Some(mock)).await;
    let dir = tempfile::tempdir().expect("temporary directory");
    let workspace = dir.path().display().to_string();

    let created = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({
            "sessionId": "default",
            "title": "写一份报告",
            "maxRounds": 4,
            "cwd": workspace,
            "acceptance": [{ "kind": "fileExists", "path": "report.md" }],
        }),
    )
    .await
    .expect("建任务");
    let id = created["task"]["id"].as_str().expect("有 id").to_string();

    let (tx, mut rx) = mpsc::unbounded_channel();
    let sink = EventSink::new(tx);
    let runner = server.clone();
    let turn = tokio::spawn(async move {
        crate::turn::run_turn(
            &runner,
            TurnStartParams {
                attachments: None,
                session_id: Some("default".to_string()),
                message: "开始吧".to_string(),
                stream: Some(true),
                cwd: None,
                tier: None,
            },
            RequestId::Number(1),
            &sink,
        )
        .await
    });

    let mut deltas = 0usize;
    let mut turn_id = String::new();
    while let Some(frame) = rx.recv().await {
        let Frame::Notification(notification) = frame else {
            continue;
        };
        match notification {
            ServerNotification::TurnStarted(started) => turn_id = started.turn_id,
            ServerNotification::TextDelta(_) => {
                deltas += 1;
                if deltas == 2 {
                    assert!(server.interrupt_turn(&turn_id), "打断该命中正在跑的回合");
                }
            }
            ServerNotification::TurnCompleted(_) => break,
            _ => {}
        }
    }
    turn.await
        .expect("回合任务不该崩")
        .expect("打断是一次正常收尾，不是错误");

    let row = server.tasks.get(&id).await.expect("查").expect("还在");
    assert_eq!(
        row.status,
        joyczl_state::TaskStatus::Suspended,
        "被打断的任务停住，等人按继续：{:?}",
        row.last_stop_reason
    );
    assert_eq!(row.rounds_used, 1, "只花了被打断的那一轮");
    let runs = server.runs.list_by_task(&id, 10).await.expect("查执行");
    assert_eq!(runs[0].status, "suspended");
}

/// P1 planner (JOY_PLAN): one extra call before the first round, and its steps land in the
/// ledger as `kind = "plan"` — outside the run, so they can never be mistaken for tool calls or
/// inflate `toolsOk`. Acceptance stays exactly what the human wrote.
#[tokio::test]
async fn the_planner_writes_plan_steps_without_touching_acceptance() {
    let mock = Arc::new(Mock::new(vec![
        // The plan call comes first: the gate, then the model's own round.
        Mock::text("{\"steps\": [\"先写骨架\", \"再补结论\"]}"),
        Mock::text("skip"),
        Mock::tool_use(
            "call_1",
            "write_file",
            json!({ "path": "report.md", "content": "结论：可以上线\n" }),
        ),
        Mock::text("写好了。"),
    ]));
    let (server, _home) = boot_with(Some(mock), true).await;
    let dir = tempfile::tempdir().expect("temporary directory");
    let workspace = dir.path().display().to_string();

    let created = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({
            "sessionId": "default",
            "title": "写一份发布报告",
            "cwd": workspace,
            "acceptance": [{ "kind": "fileExists", "path": "report.md" }],
        }),
    )
    .await
    .expect("建任务");
    let id = created["task"]["id"].as_str().expect("有 id").to_string();

    drive(&server, "default", "开始吧", None).await;

    let row = server.tasks.get(&id).await.expect("查").expect("还在");
    assert_eq!(row.status, joyczl_state::TaskStatus::Done);
    // Re-planning is allowed to rewrite the plan; acceptance is not — it is still the human's.
    let acceptance: serde_json::Value =
        serde_json::from_str(&row.acceptance_json).expect("验收条件");
    assert_eq!(acceptance.as_array().expect("数组").len(), 1);

    let planned = server.steps.list_plan(&id).await.expect("查计划");
    assert_eq!(planned.len(), 2, "两步计划各一行");
    assert!(planned.iter().all(|step| step.kind == "plan"));
    assert!(planned.iter().all(|step| step.status == "planned"));
    assert!(
        planned.iter().all(|step| step.run_id.is_none()),
        "计划不属于某一次执行"
    );
    assert!(planned[0]
        .input_json
        .as_deref()
        .is_some_and(|json| json.contains("先写骨架")));

    // The run's own steps stay tool-only: the plan is not an execution.
    let runs = server.runs.list_by_task(&id, 10).await.expect("查执行");
    let tool_steps = server.steps.list_by_run(&runs[0].id).await.expect("查步骤");
    assert_eq!(tool_steps.len(), 1);
    assert_eq!(tool_steps[0].kind, "tool");
}

/// A planner that answers nonsense must not take the task down: the round runs without a plan.
#[tokio::test]
async fn a_broken_plan_is_ignored_not_fatal() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text("我觉得可以这样做（并不打算给 JSON）"),
        Mock::text("skip"),
        Mock::tool_use(
            "call_1",
            "write_file",
            json!({ "path": "report.md", "content": "结论\n" }),
        ),
        Mock::text("写好了。"),
    ]));
    let (server, _home) = boot_with(Some(mock), true).await;
    let dir = tempfile::tempdir().expect("temporary directory");
    let workspace = dir.path().display().to_string();

    let created = call(
        &server,
        joyczl_protocol::methods::TASK_CREATE,
        json!({
            "sessionId": "default",
            "title": "写一份报告",
            "cwd": workspace,
            "acceptance": [{ "kind": "fileExists", "path": "report.md" }],
        }),
    )
    .await
    .expect("建任务");
    let id = created["task"]["id"].as_str().expect("有 id").to_string();

    drive(&server, "default", "开始吧", None).await;

    let row = server.tasks.get(&id).await.expect("查").expect("还在");
    assert_eq!(
        row.status,
        joyczl_state::TaskStatus::Done,
        "计划器坏了不影响任务本身"
    );
    assert!(server
        .steps
        .list_plan(&id)
        .await
        .expect("查计划")
        .is_empty());
}
