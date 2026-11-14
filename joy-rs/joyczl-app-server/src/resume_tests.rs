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
    let dir = tempfile::tempdir().expect("temporary directory");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("open the store");
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
        .expect("open the store");
    sqlx::query("UPDATE turn_checkpoints SET updated_at = datetime('now', ?) WHERE turn_id = ?")
        .bind(modifier)
        .bind(turn_id)
        .execute(&pool)
        .await
        .expect("change the timestamp");
}

/// The working memory a killed process would have left behind: user asked, assistant
/// requested a tool, the result came back — then the process died before the next call.
fn crashed_working_memory() -> String {
    serde_json::to_string(&vec![
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "watch the server for me for three days".to_string(),
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tu_1".to_string(),
                name: "save_note".to_string(),
                input: json!({"subject": "the server", "content": "watch it for three days"}),
                extra: None,
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tu_1".to_string(),
                content: "noted".to_string(),
            }],
        },
    ])
    .expect("serialize")
}

#[tokio::test]
async fn an_open_checkpoint_resumes_and_gets_full_accounting() {
    let mock = Arc::new(Mock::new(vec![Mock::text(
        "carrying on from where it stopped, the watching is done.",
    )]));
    let server = server(Some(mock.clone())).await;

    // The crash leftover, exactly as the checkpoint outlet leaves it.
    server
        .checkpoints
        .save(
            "t-crash",
            "test",
            "watch the server for me for three days",
            &crashed_working_memory(),
            2,
            r#"{"input_tokens":100,"output_tokens":20}"#,
            None,
        )
        .await
        .expect("write the checkpoint");

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
    assert!(completed, "the resume did not run through");

    // Announced honestly: the same turn started again, then resumed with its iteration count.
    assert!(matches!(
        notifications.first(),
        Some(ServerNotification::TurnStarted(started))
            if started.turn_id == "t-crash" && started.user_message == "watch the server for me for three days"
    ));
    assert!(notifications.iter().any(|n| matches!(
        n,
        ServerNotification::TurnResumed(resumed)
            if resumed.from_iterations == 2 && resumed.reason == "reconcile: crash recovery"
    )));

    // No finished tool re-ran: the script had exactly one response, so exactly one model call.
    // Copy out before any await — the guard cannot cross it.
    let (calls, message_count) = {
        let received = mock.received.lock().expect("lock");
        (received.len(), received.first().map(|r| r.messages.len()))
    };
    assert_eq!(calls, 1);
    assert_eq!(
        message_count,
        Some(3),
        "the request carries the checkpoint's working memory"
    );

    // Full accounting: the reply persisted under the original turn, the checkpoint closed.
    let history = server
        .chat
        .session_history("test")
        .await
        .expect("read the conversation log");
    assert_eq!(
        history.len(),
        1,
        "a resumed turn lands in the chat log as usual"
    );
    assert!(history[0].1.contains("the watching is done"));
    assert!(
        server
            .checkpoints
            .open_rows()
            .await
            .expect("read the register")
            .is_empty(),
        "a resume closes as soon as it is done"
    );
}

#[tokio::test]
async fn without_a_model_the_checkpoint_stays_open_for_later() {
    let server = server(None).await;
    server
        .checkpoints
        .save("t-wait", "test", "a long task", "[]", 1, "{}", None)
        .await
        .expect("write the checkpoint");

    let (tx, _rx) = mpsc::unbounded_channel();
    resume::reconcile(&server, &EventSink::new(tx)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        server
            .checkpoints
            .open_rows()
            .await
            .expect("read the register")
            .len(),
        1,
        "the model is not configured: keep it for now and resume once it is"
    );
}

#[tokio::test]
async fn unreadable_working_memory_is_abandoned_not_resumed() {
    let mock = Arc::new(Mock::new(vec![Mock::text("whatever")]));
    let server = server(Some(mock)).await;
    server
        .checkpoints
        .save(
            "t-broken",
            "test",
            "a long task",
            "not json at all",
            1,
            "{}",
            None,
        )
        .await
        .expect("write the checkpoint");

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
        .expect("read the register")
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
        .save(
            "t-doomed",
            "test",
            "a task that is bound to fail",
            "[]",
            1,
            "{}",
            None,
        )
        .await
        .expect("write the checkpoint");

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
        .expect("read the register")
        .is_empty());
}

/// A turn left over two days ago is **not** resumed: it is marked abandoned, rather than
/// answering again for the user on every start.
///
/// That rule comes from a real wrong answer: the user asked on 11-10 for "a briefing on
/// today's tech news", and on 11-12 the start-up resumed it — "today" had become a different
/// today, and it burned forty-odd thousand input tokens fetching news. A checkpoint is crash
/// recovery, and the window is seconds to minutes; across days it is no longer "the same turn
/// carrying on".
#[tokio::test]
async fn a_checkpoint_past_the_age_limit_is_abandoned_not_resumed() {
    let (server, home) = server_with_path(None).await;
    server
        .checkpoints
        .save(
            "t-stale",
            "test",
            "give me a briefing on today's tech news",
            "[]",
            1,
            "{}",
            None,
        )
        .await
        .expect("write the checkpoint");
    back_date(&home, "t-stale", "-2 days").await;

    let (tx, _rx) = mpsc::unbounded_channel();
    resume::reconcile(&server, &EventSink::new(tx)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let row = server
        .checkpoints
        .get("t-stale")
        .await
        .expect("read")
        .expect("present");
    assert_eq!(
        row.status, "abandoned",
        "an over-age turn must not be resumed automatically"
    );
    assert!(
        server
            .checkpoints
            .open_rows()
            .await
            .expect("read")
            .is_empty(),
        "once marked abandoned there is nothing left to resume — the next boot must not print that line again"
    );
}

/// A limit of 0 switches the rule off: back to "always resume", and an over-age row stays
/// `open` for the next round.
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
        .save(
            "t-old",
            "test",
            "something from two days ago",
            "[]",
            1,
            "{}",
            None,
        )
        .await
        .expect("write the checkpoint");
    back_date(&home, "t-old", "-2 days").await;

    let (tx, _rx) = mpsc::unbounded_channel();
    resume::reconcile(&server, &EventSink::new(tx)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let row = server
        .checkpoints
        .get("t-old")
        .await
        .expect("read")
        .expect("present");
    assert_eq!(row.status, "open", "0 means no limit, not never resume");
}

/// An unreadable timestamp is treated as "unknown": resume as before, and say so on stderr.
/// Refusing to resume makes the user ask again for nothing; resuming at worst replays the old
/// behaviour once.
#[test]
fn an_unreadable_stamp_is_not_evidence_of_age() {
    let now = chrono::NaiveDateTime::parse_from_str("2026-11-12 06:00:00", "%Y-%m-%d %H:%M:%S")
        .expect("parse");
    assert_eq!(
        super::resume::age_minutes("not a time", now),
        None,
        "an unreadable timestamp is unknown"
    );
    assert_eq!(
        super::resume::age_minutes("2026-11-12 05:30:00", now),
        Some(30)
    );
    // A clock jumping backwards (a future timestamp) gives a negative age, which must not count
    // as over-age either.
    assert_eq!(
        super::resume::age_minutes("2026-11-12 07:00:00", now),
        Some(-60)
    );
}

/// Human granularity, minutes to days, rounded down — "2 days ago" must not read as one day.
#[test]
fn ages_read_as_minutes_hours_and_days() {
    assert_eq!(super::resume::humanize(0), "0 minutes");
    assert_eq!(super::resume::humanize(59), "59 minutes");
    assert_eq!(super::resume::humanize(60), "1 hours");
    assert_eq!(super::resume::humanize(119), "1 hours");
    assert_eq!(super::resume::humanize(1_439), "23 hours");
    assert_eq!(super::resume::humanize(1_440), "1 days");
    assert_eq!(super::resume::humanize(2_880), "2 days");
}

#[tokio::test]
async fn recovered_task_must_pass_acceptance_before_done() {
    for (file_exists, budget, goal, expected) in [
        (false, "{}", None, joyczl_state::TaskStatus::NeedsAttention),
        (true, "{}", None, joyczl_state::TaskStatus::Done),
        (
            true,
            r#"{"tokens":50}"#,
            None,
            joyczl_state::TaskStatus::NeedsAttention,
        ),
        (
            true,
            "{}",
            Some("confirm the report's quality"),
            joyczl_state::TaskStatus::NeedsAttention,
        ),
    ] {
        let mock = Arc::new(Mock::new(vec![Mock::text("the report is finished.")]));
        let (server, home) = server_with_path(Some(mock.clone())).await;
        if file_exists {
            std::fs::write(home.join("report.md"), "the report").expect("the file");
        }
        server
            .tasks
            .create(joyczl_state::NewTask {
                id: "recovered-report",
                session_id: Some("test"),
                title: "write the report",
                goal_condition: goal,
                acceptance_json: r#"[{"kind":"fileExists","path":"report.md"}]"#,
                budget_json: budget,
                cwd: home.to_str(),
                max_rounds: 1,
                tier: None,
            })
            .await
            .expect("the task");
        server
            .runs
            .start(
                "t-report",
                "recovered-report",
                Some("test"),
                Some("t-report"),
            )
            .await
            .expect("the run record");
        server
            .checkpoints
            .save_round(
                "t-report",
                "test",
                "write the report",
                "[]",
                1,
                r#"{"input_tokens":90,"output_tokens":0}"#,
                Some("t-report"),
                Some("report-round"),
            )
            .await
            .expect("the checkpoint");
        server
            .runs
            .account_round(
                "t-report",
                "report-round",
                &joyczl_state::runs::RoundAccount {
                    tokens_in: 100,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        resume::reconcile(&server, &EventSink::new(tx)).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(frame) = rx.recv().await {
                if matches!(&frame, Frame::Notification(ServerNotification::TaskUpdated(update))
                    if budget != "{}" && update.status == "needs_attention")
                    || matches!(
                        frame,
                        Frame::Notification(ServerNotification::TurnCompleted(_))
                    )
                {
                    return;
                }
            }
            panic!("not finished");
        })
        .await
        .expect("the recovery times out");
        let task = server
            .tasks
            .get("recovered-report")
            .await
            .expect("query")
            .expect("it exists");
        assert_eq!(task.status, expected);
        if budget == "{}" {
            assert_eq!(task.spent_tokens, 115);
            assert!(task.checks_json.contains("report.md"));
        } else {
            assert_eq!(task.spent_tokens, 100);
            assert!(mock.received.lock().unwrap().is_empty());
        }
        assert_eq!(task.spent_tools, 0);
    }
}

#[tokio::test]
async fn pending_tool_intent_is_not_replayed() {
    let mock = Arc::new(Mock::new(vec![Mock::text("must not run")]));
    let server = server(Some(mock.clone())).await;
    let mut messages: Vec<Message> = serde_json::from_str(&crashed_working_memory()).unwrap();
    messages.pop();
    server
        .checkpoints
        .save(
            "unknown",
            "test",
            "work",
            &serde_json::to_string(&messages).unwrap(),
            0,
            "{}",
            None,
        )
        .await
        .unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    resume::reconcile(&server, &EventSink::new(tx)).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if server
                .checkpoints
                .get("unknown")
                .await
                .unwrap()
                .unwrap()
                .status
                == "suspended"
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(mock.received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn recovered_goal_uses_the_shared_driver_until_satisfied() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text("First half complete."),
        Mock::text(r#"{"ok":false,"impossible":false,"reason":"finish the remaining half"}"#),
        Mock::text(r#"{"retrieve":false,"query":"","reason":"goal"}"#),
        Mock::text("Both halves complete."),
        Mock::text(r#"{"ok":true,"impossible":false,"reason":"verified"}"#),
    ]));
    let (server, _) = server_with_path(Some(mock.clone())).await;
    server
        .tasks
        .create(joyczl_state::NewTask {
            id: "resume-goal",
            session_id: Some("test"),
            title: "two halves",
            goal_condition: Some("finish both halves"),
            acceptance_json: "[]",
            budget_json: "{}",
            cwd: None,
            max_rounds: 2,
            tier: None,
        })
        .await
        .unwrap();
    server
        .runs
        .start("goal-run", "resume-goal", Some("test"), Some("goal-run"))
        .await
        .unwrap();
    server
        .checkpoints
        .save_round(
            "goal-run",
            "test",
            "start",
            "[]",
            1,
            r#"{"input_tokens":0,"output_tokens":0}"#,
            Some("goal-run"),
            Some("goal-round-1"),
        )
        .await
        .unwrap();
    joyczl_agent::plan::record(
        &server.steps,
        "resume-goal",
        &joyczl_agent::plan::Plan {
            version: 2,
            steps: vec!["Use the restored evidence ledger".into()],
        },
    )
    .await
    .unwrap();
    let (sink, mut receiver) = EventSink::channel();
    resume::reconcile(&server, &sink).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(frame) = receiver.recv().await {
            if let Frame::Notification(ServerNotification::TurnCompleted(done)) = frame {
                assert_eq!(done.meta.goal_status.as_deref(), Some("satisfied"));
                assert_eq!(done.meta.goal_rounds, 2);
                return;
            }
        }
        panic!("recovered goal never completed");
    })
    .await
    .unwrap();
    let task = server.tasks.get("resume-goal").await.unwrap().unwrap();
    assert_eq!(task.status, joyczl_state::TaskStatus::Done);
    assert_eq!(task.rounds_used, 2);
    assert_eq!(
        server.runs.get("goal-run").await.unwrap().unwrap().rounds,
        2
    );
    let requests = mock.received.lock().unwrap();
    assert_eq!(requests.len(), 5);
    assert!(requests[0]
        .system_text()
        .unwrap()
        .contains("finish both halves"));
    assert!(requests[3]
        .system_text()
        .unwrap()
        .contains("finish both halves"));

    assert!(
        requests[3]
            .messages
            .iter()
            .any(|m| m.text().contains("Use the restored evidence ledger")),
        "goal continuation must retain the persisted plan"
    );

    assert!(requests.iter().any(|request| request
        .messages
        .iter()
        .any(|m| m.text().contains("finish the remaining half"))));
}

#[tokio::test]
async fn recovery_uses_the_checkpoint_round_cap_instead_of_a_stale_task_counter() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text("Second round result."),
        Mock::text(r#"{"ok":false,"impossible":false,"reason":"still incomplete"}"#),
    ]));
    let (server, _) = server_with_path(Some(mock.clone())).await;
    server
        .tasks
        .create(joyczl_state::NewTask {
            id: "round-cap",
            session_id: Some("test"),
            title: "work",
            goal_condition: Some("finish work"),
            acceptance_json: "[]",
            budget_json: "{}",
            cwd: None,
            max_rounds: 2,
            tier: None,
        })
        .await
        .unwrap();
    server
        .runs
        .start("round-run", "round-cap", Some("test"), Some("round-turn"))
        .await
        .unwrap();
    server
        .checkpoints
        .save_numbered_round(
            "round-turn",
            "test",
            "continue work",
            "[]",
            0,
            "{}",
            Some("round-run"),
            Some("second-round"),
            Some(2),
        )
        .await
        .unwrap();
    server.tasks.record_round("round-cap", 1).await.unwrap();
    let (sink, mut receiver) = EventSink::channel();
    resume::reconcile(&server, &sink).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(frame) = receiver.recv().await {
            if let Frame::Notification(ServerNotification::TurnCompleted(done)) = frame {
                assert_eq!(done.meta.goal_rounds, 2);
                assert_eq!(done.meta.goal_status.as_deref(), Some("round-limit"));
                return;
            }
        }
        panic!("recovery never completed");
    })
    .await
    .unwrap();
    assert_eq!(mock.received.lock().unwrap().len(), 2);
    assert_eq!(
        server
            .tasks
            .get("round-cap")
            .await
            .unwrap()
            .unwrap()
            .rounds_used,
        2
    );
}
