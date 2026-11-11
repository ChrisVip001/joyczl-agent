//! The goal loop: set/clear, the judge's three-state validation, and the closed loop of "not met → one more round → met".
//!
//! The judge and the main loop share the same model (a Mock in tests), so the scripted queue is the
//! **real call order**: gate → main reply → judge → gate → main reply → judge.

use std::sync::Arc;

use joyczl_config::Settings;
use joyczl_protocol::{RequestId, ServerNotification, TurnStartParams};
use joyczl_provider::mock::Mock;
use joyczl_provider::Resolved;

use crate::{EventSink, Server};

fn resolved_with(mock: Arc<Mock>) -> Resolved {
    Resolved {
        context_window: joyczl_provider::DEFAULT_CONTEXT_WINDOW,
        vision: false,
        provider_id: "mock".to_string(),
        client: mock,
        model: "test-model".to_string(),
        small_model: "test-small".to_string(),
    }
}

async fn server(mock: Arc<Mock>, goal_max_rounds: i32) -> (Server, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    let server = Server::boot(
        pool,
        Settings {
            home,
            goal_max_rounds,
            ..Settings::default()
        },
    )
    .await;
    server.set_resolved(Some(resolved_with(mock)));
    (server, dir)
}

/// Setting a goal **is** creating a task: one row, no second record to disagree with it.
#[tokio::test]
async fn a_goal_is_the_task_row_and_nothing_else() {
    let (server, _dir) = server(Arc::new(Mock::new(Vec::new())), 5).await;

    let row = server
        .set_task_goal("s1", Some("  让测试全绿  "))
        .await
        .expect("设目标该建任务");
    assert_eq!(
        row.goal_condition.as_deref(),
        Some("让测试全绿"),
        "两侧空白该被去掉"
    );
    assert_eq!(row.rounds_used, 0);
    assert_eq!(row.max_rounds, 5, "上限来自 settings");

    // Setting again edits that row — it must not stack a second task on the session.
    let same = server
        .set_task_goal("s1", Some("让 clippy 也绿"))
        .await
        .expect("任务仍在");
    assert_eq!(same.id, row.id);
    assert_eq!(same.goal_condition.as_deref(), Some("让 clippy 也绿"));

    // Clearing takes the condition off and leaves the row (its title is the record).
    let cleared = server
        .set_task_goal("s1", Some("   "))
        .await
        .expect("任务仍在");
    assert_eq!(cleared.id, row.id);
    assert!(cleared.goal_condition.is_none(), "目标清掉了");

    // A session with nothing: clearing is a no-op, not an error and not a new row.
    assert!(server.set_task_goal("s2", None).await.is_none());
}

/// `ok` and `impossible` both true = a muddled judge → counts as not done (and says so).
#[tokio::test]
async fn a_self_contradicting_judgement_counts_as_not_done() {
    let mock = Arc::new(Mock::new(vec![Mock::text(
        r#"{"ok": true, "impossible": true, "reason": "说不清"}"#,
    )]));
    let resolved = resolved_with(mock);
    let judgement = crate::goal::judge(&resolved, "目标", "对话")
        .await
        .expect("判断出来了");
    assert!(!judgement.ok && !judgement.impossible, "{judgement:?}");
    assert!(judgement.reason.contains("自相矛盾"), "{judgement:?}");
}

/// The judge gives no JSON: an error (the caller stops continuing and keeps the goal), not a fake success.
#[tokio::test]
async fn a_judge_that_does_not_answer_json_is_an_error() {
    let mock = Arc::new(Mock::new(vec![Mock::text("我觉得差不多了吧")]));
    let resolved = resolved_with(mock);
    let outcome = crate::goal::judge(&resolved, "目标", "对话").await;
    assert!(outcome.is_err(), "没 JSON 就该是错误：{outcome:?}");
}

/// The closed loop: not met → one more round → met. `TurnMeta` and notifications must reflect it honestly.
#[tokio::test]
async fn the_loop_continues_until_the_judge_is_satisfied() {
    let mock = Arc::new(Mock::new(vec![
        // First round: gate → main reply → judge (not met)
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "goal"}"#),
        Mock::text("我先做了一半。"),
        Mock::text(r#"{"ok": false, "impossible": false, "reason": "测试还有两条红"}"#),
        // Second round: gate → main reply → judge (met)
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "goal"}"#),
        Mock::text("两条都修好了。"),
        Mock::text(r#"{"ok": true, "impossible": false, "reason": "全绿了"}"#),
    ]));
    let (server, _dir) = server(mock.clone(), 5).await;
    server
        .set_task_goal("goal-session", Some("让测试全绿"))
        .await
        .expect("设上");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    crate::turn::run_turn(
        &server,
        TurnStartParams {
            attachments: None,
            session_id: Some("goal-session".to_string()),
            message: "开始吧".to_string(),
            stream: Some(true),
            cwd: None,
            tier: None,
        },
        RequestId::Number(1),
        &EventSink::new(tx),
    )
    .await
    .expect("这一轮该跑完");

    let mut rounds = Vec::new();
    let mut meta = None;
    let mut reply = String::new();
    while let Ok(frame) = rx.try_recv() {
        match frame {
            crate::Frame::Notification(ServerNotification::GoalRound(round)) => {
                rounds.push((round.status.clone(), round.reason.clone(), round.round));
            }
            crate::Frame::Notification(ServerNotification::TurnCompleted(done)) => {
                meta = Some(done.meta.clone());
                reply = done.reply.clone();
            }
            _ => {}
        }
    }

    assert_eq!(rounds.len(), 2, "续一轮 + 达成一次：{rounds:?}");
    assert_eq!(rounds[0].0, "continuing", "{rounds:?}");
    assert_eq!(rounds[1].0, "satisfied", "{rounds:?}");
    assert_eq!(rounds[1].2, 2, "第二轮：{rounds:?}");
    assert!(
        rounds[0].1.contains("还有两条红"),
        "理由要带过去：{rounds:?}"
    );

    let meta = meta.expect("有 TurnCompleted");
    assert_eq!(meta.goal_status.as_deref(), Some("satisfied"), "{meta:?}");
    assert_eq!(meta.goal_rounds, 2, "{meta:?}");
    assert!(reply.contains("修好了"), "最终答复是第二轮的：{reply}");

    // The judge's **reason** is fed back as the user message and the turn continues (findable in the same session's history).
    let requests = mock.received.lock().unwrap();
    assert!(
        requests
            .iter()
            .any(|request| request.messages.iter().any(|message| {
                message
                    .content
                    .iter()
                    .any(|block| matches!(block, joyczl_provider::ContentBlock::Text { text } if text.contains("还有两条红")))
            })),
        "续轮要把理由喂回去"
    );
}

/// Cap reached without success: honestly stamped `round-limit`, no faked completion.
#[tokio::test]
async fn running_out_of_rounds_is_reported_honestly() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "goal"}"#),
        Mock::text("还没弄好。"),
        Mock::text(r#"{"ok": false, "impossible": false, "reason": "还差一点"}"#),
    ]));
    let (server, _dir) = server(mock, 1).await;
    server
        .set_task_goal("goal-session", Some("做到 X"))
        .await
        .expect("设上");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    crate::turn::run_turn(
        &server,
        TurnStartParams {
            attachments: None,
            session_id: Some("goal-session".to_string()),
            message: "开始".to_string(),
            stream: Some(true),
            cwd: None,
            tier: None,
        },
        RequestId::Number(1),
        &EventSink::new(tx),
    )
    .await
    .expect("跑完");

    let mut statuses = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        if let crate::Frame::Notification(ServerNotification::GoalRound(round)) = frame {
            statuses.push(round.status);
        }
    }
    assert_eq!(statuses, vec!["round-limit".to_string()], "一轮就是上限");
    let row = server
        .tasks
        .latest("goal-session")
        .await
        .expect("查")
        .expect("任务还在");
    assert_eq!(row.status, joyczl_state::TaskStatus::Failed, "不伪装完成");
    assert_eq!(
        row.goal_condition.as_deref(),
        Some("做到 X"),
        "也不清掉目标：停下等人决定"
    );
}
