//! Startup reconciliation tests — the crash-recovery half answers one question:
//! **does a fresh process actually continue, and does it continue honestly**.
//!
//! The "previous process" is simulated by writing a checkpoint row the way the checkpoint
//! outlet would have left it after a kill -9 mid-turn: `open`, working memory serialized.

use std::sync::Arc;
use std::time::Duration;

use joyczl_config::Settings;
use joyczl_protocol::ServerNotification;
use joyczl_provider::mock::Mock;
use joyczl_provider::{ContentBlock, Message, Resolved, Role};
use serde_json::json;
use tokio::sync::mpsc;

use crate::{resume, EventSink, Frame, Server};

/// A server with the scripted fake provider swapped in (same recipe as turn_tests),
/// plus its home — the age-limit tests need a second connection to back-date a row.
async fn server_with_path(mock: Option<Arc<Mock>>) -> (Server, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    let settings = Settings {
        home: home.clone(),
        ..Settings::default()
    };
    let _ = dir.keep(); // sqlite still writes -wal/-shm
    let server = Server::boot(pool, settings).await;
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

async fn server(mock: Option<Arc<Mock>>) -> Server {
    server_with_path(mock).await.0
}

/// Back-date a checkpoint without waiting two days.
async fn back_date(home: &std::path::Path, turn_id: &str, modifier: &str) {
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    sqlx::query("UPDATE turn_checkpoints SET updated_at = datetime('now', ?) WHERE turn_id = ?")
        .bind(modifier)
        .bind(turn_id)
        .execute(&pool)
        .await
        .expect("改时间戳");
}

/// The working memory a killed process would have left behind: user asked, assistant
/// requested a tool, the result came back — then the process died before the next call.
fn crashed_working_memory() -> String {
    serde_json::to_string(&vec![
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "帮我盯三天服务器".to_string(),
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tu_1".to_string(),
                name: "save_note".to_string(),
                input: json!({"subject": "服务器", "content": "要盯三天"}),
                extra: None,
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tu_1".to_string(),
                content: "已记录".to_string(),
            }],
        },
    ])
    .expect("序列化")
}

#[tokio::test]
async fn an_open_checkpoint_resumes_and_gets_full_accounting() {
    let mock = Arc::new(Mock::new(vec![Mock::text("接着上次的进度，已经盯完了。")]));
    let server = server(Some(mock.clone())).await;

    // The crash leftover, exactly as the checkpoint outlet leaves it.
    server
        .checkpoints
        .save(
            "t-crash",
            "test",
            "帮我盯三天服务器",
            &crashed_working_memory(),
            2,
            r#"{"input_tokens":100,"output_tokens":20}"#,
            None,
        )
        .await
        .expect("写 checkpoint");

    let (tx, mut rx) = mpsc::unbounded_channel();
    resume::reconcile(&server, &EventSink::new(tx)).await;

    // Resumed turns run as their own tasks: drain until the turn completes.
    let mut notifications: Vec<ServerNotification> = Vec::new();
    let mut completed = false;
    for _ in 0..200 {
        while let Ok(frame) = rx.try_recv() {
            if let Frame::Notification(n) = frame {
                completed |= matches!(n, ServerNotification::TurnCompleted(_));
                notifications.push(n);
            }
        }
        if completed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(completed, "续跑没走完");

    // Announced honestly: the same turn started again, then resumed with its iteration count.
    assert!(matches!(
        notifications.first(),
        Some(ServerNotification::TurnStarted(started))
            if started.turn_id == "t-crash" && started.user_message == "帮我盯三天服务器"
    ));
    assert!(notifications.iter().any(|n| matches!(
        n,
        ServerNotification::TurnResumed(resumed)
            if resumed.from_iterations == 2 && resumed.reason == "reconcile: crash recovery"
    )));

    // No finished tool re-ran: the script had exactly one response, so exactly one model call.
    // Copy out before any await — the guard cannot cross it.
    let (calls, message_count) = {
        let received = mock.received.lock().expect("锁");
        (received.len(), received.first().map(|r| r.messages.len()))
    };
    assert_eq!(calls, 1);
    assert_eq!(message_count, Some(3), "请求带的就是 checkpoint 的工作记忆");

    // Full accounting: the reply persisted under the original turn, the checkpoint closed.
    let history = server
        .chat
        .session_history("test")
        .await
        .expect("读对话日志");
    assert_eq!(history.len(), 1, "续跑的回合照常落对话日志");
    assert!(history[0].1.contains("已经盯完了"));
    assert!(
        server
            .checkpoints
            .open_rows()
            .await
            .expect("读台账")
            .is_empty(),
        "续跑完成即关闭"
    );
}

#[tokio::test]
async fn without_a_model_the_checkpoint_stays_open_for_later() {
    let server = server(None).await;
    server
        .checkpoints
        .save("t-wait", "test", "长任务", "[]", 1, "{}", None)
        .await
        .expect("写 checkpoint");

    let (tx, _rx) = mpsc::unbounded_channel();
    resume::reconcile(&server, &EventSink::new(tx)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        server.checkpoints.open_rows().await.expect("读台账").len(),
        1,
        "模型没配好：先留着，配好再续"
    );
}

#[tokio::test]
async fn unreadable_working_memory_is_abandoned_not_resumed() {
    let mock = Arc::new(Mock::new(vec![Mock::text("随便")]));
    let server = server(Some(mock)).await;
    server
        .checkpoints
        .save(
            "t-broken",
            "test",
            "长任务",
            "not json at all",
            1,
            "{}",
            None,
        )
        .await
        .expect("写 checkpoint");

    let (tx, _rx) = mpsc::unbounded_channel();
    resume::reconcile(&server, &EventSink::new(tx)).await;
    for _ in 0..100 {
        if server
            .checkpoints
            .open_rows()
            .await
            .unwrap_or_default()
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(server
        .checkpoints
        .open_rows()
        .await
        .expect("读台账")
        .is_empty());
}

/// A turn that fails inside its process closes its checkpoint — after that, `open` can only
/// mean a crash, which is the invariant reconcile stands on. (The mock errors on every call.)
#[tokio::test]
async fn a_turn_error_marks_its_checkpoint_abandoned() {
    let mock = Arc::new(Mock::new(vec![]));
    let server = server(Some(mock)).await;
    server
        .checkpoints
        .save("t-doomed", "test", "必败的任务", "[]", 1, "{}", None)
        .await
        .expect("写 checkpoint");

    let (tx, _rx) = mpsc::unbounded_channel();
    resume::reconcile(&server, &EventSink::new(tx)).await;
    for _ in 0..100 {
        if server
            .checkpoints
            .open_rows()
            .await
            .unwrap_or_default()
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(server
        .checkpoints
        .open_rows()
        .await
        .expect("读台账")
        .is_empty());
}

/// 两天前留下的回合**不**自动续跑：标 abandoned，而不是每次启动都替人再答一遍。
///
/// 这条规则来自一次真实的误答：用户 11-10 问「给我一份今天科技新闻的简报」，11-12 启动时
/// 被续跑，「今天」变成了另一个今天，还花了四万多输入 token 去抓新闻。checkpoint 是崩溃
/// 恢复，窗口是秒到分钟；跨天之后它已经不是「同一个回合接着跑」了。
#[tokio::test]
async fn a_checkpoint_past_the_age_limit_is_abandoned_not_resumed() {
    let (server, home) = server_with_path(None).await;
    server
        .checkpoints
        .save(
            "t-stale",
            "test",
            "给我一份今天科技新闻的简报",
            "[]",
            1,
            "{}",
            None,
        )
        .await
        .expect("写 checkpoint");
    back_date(&home, "t-stale", "-2 days").await;

    let (tx, _rx) = mpsc::unbounded_channel();
    resume::reconcile(&server, &EventSink::new(tx)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let row = server
        .checkpoints
        .get("t-stale")
        .await
        .expect("读")
        .expect("在");
    assert_eq!(row.status, "abandoned", "超龄的回合不该被自动续跑");
    assert!(
        server.checkpoints.open_rows().await.expect("读").is_empty(),
        "标完 abandoned 之后就没有可续的了 —— 下次启动不该再打印同一行"
    );
}

/// 上限设成 0 = 关掉这条规则：回归到「一律续上」，超龄行仍然 `open` 等下一轮。
#[tokio::test]
async fn a_zero_limit_keeps_the_old_resume_everything_behaviour() {
    let (server, home) = server_with_path(None).await;
    server.set_settings(Settings {
        home: home.clone(),
        resume_max_age_minutes: 0,
        ..Settings::default()
    });
    server
        .checkpoints
        .save("t-old", "test", "两天前的事", "[]", 1, "{}", None)
        .await
        .expect("写 checkpoint");
    back_date(&home, "t-old", "-2 days").await;

    let (tx, _rx) = mpsc::unbounded_channel();
    resume::reconcile(&server, &EventSink::new(tx)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let row = server
        .checkpoints
        .get("t-old")
        .await
        .expect("读")
        .expect("在");
    assert_eq!(row.status, "open", "0 是「不设上限」，不是「一律不续」");
}

/// 时间戳读不出来时按「不知道」处理：照旧续跑，并把话说在 stderr 上。
/// 拒绝续跑会让人白问一遍；续跑最多是把旧行为重放一次。
#[test]
fn an_unreadable_stamp_is_not_evidence_of_age() {
    let now = chrono::NaiveDateTime::parse_from_str("2026-11-12 06:00:00", "%Y-%m-%d %H:%M:%S")
        .expect("解析");
    assert_eq!(super::resume::age_minutes("不是时间", now), None);
    assert_eq!(
        super::resume::age_minutes("2026-11-12 05:30:00", now),
        Some(30)
    );
    // 时钟往回跳（未来时间戳）得到负数，也不该被判成超龄。
    assert_eq!(
        super::resume::age_minutes("2026-11-12 07:00:00", now),
        Some(-60)
    );
}

/// 人话的粒度：从分钟到天，向下取整 —— 「2 天前」不能读成 1 天。
#[test]
fn ages_read_as_minutes_hours_and_days() {
    assert_eq!(super::resume::humanize(0), "0 分钟");
    assert_eq!(super::resume::humanize(59), "59 分钟");
    assert_eq!(super::resume::humanize(60), "1 小时");
    assert_eq!(super::resume::humanize(119), "1 小时");
    assert_eq!(super::resume::humanize(1_439), "23 小时");
    assert_eq!(super::resume::humanize(1_440), "1 天");
    assert_eq!(super::resume::humanize(2_880), "2 天");
}
