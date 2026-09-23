//! 目标循环：设/清、判断器的三态校验、以及「没达成 → 再跑一轮 → 达成」的闭环。
//!
//! 判断器与主循环共用同一个模型（测试里都是 Mock），所以脚本队列就是**真实调用
//! 顺序**：门 → 主答复 → 判断 → 门 → 主答复 → 判断。

use std::sync::Arc;

use joyczl_config::Settings;
use joyczl_protocol::{RequestId, ServerNotification, TurnStartParams};
use joyczl_provider::mock::Mock;
use joyczl_provider::Resolved;

use crate::{EventSink, Server};

fn resolved_with(mock: Arc<Mock>) -> Resolved {
    Resolved {
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

/// 设、清、记轮次、盖终止态。
#[tokio::test]
async fn setting_and_clearing_a_goal() {
    let (server, _dir) = server(Arc::new(Mock::new(Vec::new())), 5).await;
    let goals = &server.goals;

    assert!(crate::goal::active(goals, "s1").is_none(), "一开始没目标");

    let goal = crate::goal::set(goals, "s1", Some("  让测试全绿  ")).expect("设上");
    assert_eq!(goal.condition, "让测试全绿", "两侧空白该被去掉");
    assert_eq!(goal.rounds, 0);
    assert!(crate::goal::active(goals, "s1").is_some());

    assert_eq!(crate::goal::bump(goals, "s1"), 1);
    assert_eq!(crate::goal::bump(goals, "s1"), 2, "轮次跨 turn 累计");

    // 终止态之后循环不再自己跑（但记录还在 —— 人能看见它为什么停）。
    crate::goal::finish(goals, "s1", "round-limit");
    assert!(crate::goal::active(goals, "s1").is_none(), "停了就不活跃");
    assert!(crate::goal::read(goals, "s1").is_some(), "但没被清掉");

    // 空条件 = 清除。
    assert!(crate::goal::set(goals, "s1", Some("   ")).is_none());
    assert!(crate::goal::read(goals, "s1").is_none());
}

/// `ok` 与 `impossible` 同时为真 = 判断器没想清楚 → 当作没达成（并说出这件事）。
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

/// 判断器没给 JSON：报错（调用方据此停续轮并保住目标），而不是当成达成。
#[tokio::test]
async fn a_judge_that_does_not_answer_json_is_an_error() {
    let mock = Arc::new(Mock::new(vec![Mock::text("我觉得差不多了吧")]));
    let resolved = resolved_with(mock);
    let outcome = crate::goal::judge(&resolved, "目标", "对话").await;
    assert!(outcome.is_err(), "没 JSON 就该是错误：{outcome:?}");
}

/// 闭环：没达成 → 续一轮 → 达成。`TurnMeta` 与通知都要如实反映。
#[tokio::test]
async fn the_loop_continues_until_the_judge_is_satisfied() {
    let mock = Arc::new(Mock::new(vec![
        // 第一轮：门 → 主答复 → 判断（没达成）
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "goal"}"#),
        Mock::text("我先做了一半。"),
        Mock::text(r#"{"ok": false, "impossible": false, "reason": "测试还有两条红"}"#),
        // 第二轮：门 → 主答复 → 判断（达成）
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "goal"}"#),
        Mock::text("两条都修好了。"),
        Mock::text(r#"{"ok": true, "impossible": false, "reason": "全绿了"}"#),
    ]));
    let (server, _dir) = server(mock.clone(), 5).await;
    crate::goal::set(&server.goals, "goal-session", Some("让测试全绿")).expect("设上");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    crate::turn::run_turn(
        &server,
        TurnStartParams {
            session_id: Some("goal-session".to_string()),
            message: "开始吧".to_string(),
            stream: Some(true),
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

    // 判断器看到的是**理由**当成用户消息接着跑（同一会话的历史里能查到）。
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

/// 到了上限还没达成：如实置 `round-limit`，不伪装完成。
#[tokio::test]
async fn running_out_of_rounds_is_reported_honestly() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "goal"}"#),
        Mock::text("还没弄好。"),
        Mock::text(r#"{"ok": false, "impossible": false, "reason": "还差一点"}"#),
    ]));
    let (server, _dir) = server(mock, 1).await;
    crate::goal::set(&server.goals, "goal-session", Some("做到 X")).expect("设上");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    crate::turn::run_turn(
        &server,
        TurnStartParams {
            session_id: Some("goal-session".to_string()),
            message: "开始".to_string(),
            stream: Some(true),
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
    assert!(
        crate::goal::read(&server.goals, "goal-session").is_some(),
        "不伪装完成，也不清掉目标"
    );
}
