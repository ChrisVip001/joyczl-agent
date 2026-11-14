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
    let dir = tempfile::tempdir().expect("temporary directory");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("open the store");
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
        .set_task_goal("s1", Some("  get the tests green  "))
        .await
        .expect("setting a goal should create the task");
    assert_eq!(
        row.goal_condition.as_deref(),
        Some("get the tests green"),
        "surrounding whitespace has to be trimmed away"
    );
    assert_eq!(row.rounds_used, 0);
    assert_eq!(row.max_rounds, 5, "the cap comes from settings");

    // Setting again edits that row — it must not stack a second task on the session.
    let same = server
        .set_task_goal("s1", Some("get clippy green too"))
        .await
        .expect("the task is still there");
    assert_eq!(same.id, row.id);
    assert_eq!(same.goal_condition.as_deref(), Some("get clippy green too"));

    // Clearing takes the condition off and leaves the row (its title is the record).
    let cleared = server
        .set_task_goal("s1", Some("   "))
        .await
        .expect("the task is still there");
    assert_eq!(cleared.id, row.id);
    assert!(cleared.goal_condition.is_none(), "the goal was cleared");

    // A session with nothing: clearing is a no-op, not an error and not a new row.
    assert!(server.set_task_goal("s2", None).await.is_none());
}

/// `ok` and `impossible` both true = a muddled judge → counts as not done (and says so).
#[tokio::test]
async fn a_self_contradicting_judgement_counts_as_not_done() {
    let mock = Arc::new(Mock::new(vec![Mock::text(
        r#"{"ok": true, "impossible": true, "reason": "cannot say"}"#,
    )]));
    let resolved = resolved_with(mock);
    let judgement = crate::goal::judge(&resolved, "goal", "conversation")
        .await
        .expect("it was judged");
    assert!(!judgement.ok && !judgement.impossible, "{judgement:?}");
    assert!(
        judgement.reason.contains("contradicted itself"),
        "{judgement:?}"
    );
}

/// The judge gives no JSON: an error (the caller stops continuing and keeps the goal), not a fake success.
#[tokio::test]
async fn a_judge_that_does_not_answer_json_is_an_error() {
    let mock = Arc::new(Mock::new(vec![Mock::text("I think that is about enough")]));
    let resolved = resolved_with(mock);
    let outcome = crate::goal::judge(&resolved, "goal", "conversation").await;
    assert!(outcome.is_err(), "no JSON means an error: {outcome:?}");
}

/// The closed loop: not met → one more round → met. `TurnMeta` and notifications must reflect it honestly.
#[tokio::test]
async fn the_loop_continues_until_the_judge_is_satisfied() {
    let mock = Arc::new(Mock::new(vec![
        // First round: gate → main reply → judge (not met)
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "goal"}"#),
        Mock::tool_use("clock1", "current_time", serde_json::json!({})),
        Mock::text("I did half of it first."),
        Mock::text(r#"{"ok": false, "impossible": false, "reason": "two tests are still red"}"#),
        // Second round: gate → main reply → judge (met)
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "goal"}"#),
        Mock::tool_use("clock2", "current_time", serde_json::json!({})),
        Mock::text("both are fixed."),
        Mock::text(r#"{"ok": true, "impossible": false, "reason": "all green"}"#),
    ]));
    let (server, _dir) = server(mock.clone(), 5).await;
    let task = server
        .set_task_goal("goal-session", Some("get the tests green"))
        .await
        .expect("set it");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    crate::turn::run_turn(
        &server,
        TurnStartParams {
            attachments: None,
            session_id: Some("goal-session".to_string()),
            message: "let us start".to_string(),
            stream: Some(true),
            cwd: None,
            tier: None,
        },
        RequestId::Number(1),
        &EventSink::new(tx),
    )
    .await
    .expect("this turn should finish");

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

    assert_eq!(
        rounds.len(),
        2,
        "one continuation plus one achievement: {rounds:?}"
    );
    assert_eq!(rounds[0].0, "continuing", "{rounds:?}");
    assert_eq!(rounds[1].0, "satisfied", "{rounds:?}");
    assert_eq!(rounds[1].2, 2, "the second round: {rounds:?}");
    assert!(
        rounds[0].1.contains("two tests are still red"),
        "the reason has to be carried over: {rounds:?}"
    );

    let meta = meta.expect("there is a TurnCompleted");
    assert_eq!(meta.goal_status.as_deref(), Some("satisfied"), "{meta:?}");
    assert_eq!(meta.goal_rounds, 2, "{meta:?}");
    assert!(
        reply.contains("fixed"),
        "the final reply is the second round's: {reply}"
    );

    // The judge's **reason** is fed back as the user message and the turn continues (findable in the same session's history).
    assert_eq!(
        server
            .steps
            .count_ok_by_tool(&task.id, "current_time")
            .await
            .unwrap(),
        2,
        "both goal rounds must retain distinct operation records"
    );
    let requests = mock.received.lock().unwrap();
    assert!(
        requests
            .iter()
            .any(|request| request.messages.iter().any(|message| {
                message
                    .content
                    .iter()
                    .any(|block| matches!(block, joyczl_provider::ContentBlock::Text { text } if text.contains("two tests are still red")))
            })),
        "a continuation has to feed the reason back"
    );
}

/// Cap reached without success: honestly stamped `round-limit`, no faked completion.
#[tokio::test]
async fn running_out_of_rounds_is_reported_honestly() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "goal"}"#),
        Mock::text("not done yet."),
        Mock::text(r#"{"ok": false, "impossible": false, "reason": "a little short"}"#),
    ]));
    let (server, _dir) = server(mock, 1).await;
    server
        .set_task_goal("goal-session", Some("do X"))
        .await
        .expect("set it");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    crate::turn::run_turn(
        &server,
        TurnStartParams {
            attachments: None,
            session_id: Some("goal-session".to_string()),
            message: "start".to_string(),
            stream: Some(true),
            cwd: None,
            tier: None,
        },
        RequestId::Number(1),
        &EventSink::new(tx),
    )
    .await
    .expect("it finishes");

    let mut statuses = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        if let crate::Frame::Notification(ServerNotification::GoalRound(round)) = frame {
            statuses.push(round.status);
        }
    }
    assert_eq!(
        statuses,
        vec!["round-limit".to_string()],
        "one round is the limit"
    );
    let row = server
        .tasks
        .latest("goal-session")
        .await
        .expect("query")
        .expect("the task is still there");
    assert_eq!(
        row.status,
        joyczl_state::TaskStatus::Failed,
        "it does not fake being done"
    );
    assert_eq!(
        row.goal_condition.as_deref(),
        Some("do X"),
        "and it does not clear the goal either: it stops and waits for a human"
    );
}

#[tokio::test]
async fn a_truncated_answer_cannot_complete_a_goal_or_reach_the_judge() {
    let mut partial = Mock::text("The report begins...");
    partial.stop_reason = joyczl_provider::StopReason::MaxTokens;
    let mock = Arc::new(Mock::new(vec![
        Mock::text(r#"{"retrieve":false,"query":"","reason":"goal"}"#),
        partial,
    ]));
    let (server, _home) = server(mock.clone(), 2).await;
    let task = server
        .set_task_goal("goal-session", Some("finish the report"))
        .await
        .unwrap();
    let (sink, _receiver) = EventSink::channel();
    crate::turn::run_turn(
        &server,
        TurnStartParams {
            session_id: Some("goal-session".into()),
            message: "write the report".into(),
            ..Default::default()
        },
        RequestId::Number(1),
        &sink,
    )
    .await
    .unwrap();
    let task = server.tasks.get(&task.id).await.unwrap().unwrap();
    assert_eq!(task.status, joyczl_state::TaskStatus::NeedsAttention);
    assert!(task.last_stop_reason.unwrap().contains("token limit"));
    assert_eq!(mock.received.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn an_uncertain_effect_suspends_the_goal_before_any_model_retry() {
    let mock = Arc::new(Mock::new(vec![
        Mock::text(r#"{"retrieve":false,"query":"","reason":"goal"}"#),
        Mock::tool_use("call", "unknown_effect", serde_json::json!({})),
        Mock::text("must not be requested"),
    ]));
    let (mut server, _home) = server(mock.clone(), 2).await;
    let mut tools = (*server.tools).clone();
    tools.register_exclusive(joyczl_tools::Tool {
        name: "unknown_effect".into(),
        description: "test remote uncertainty".into(),
        input_schema: serde_json::json!({"type":"object"}),
        handler: Arc::new(|ctx, _| {
            Box::pin(async move {
                ctx.on_uncertain.as_ref().unwrap()("unknown remote outcome");
                Ok("Error: outcome unknown".into())
            })
        }),
    });
    server.tools = Arc::new(tools);
    let task = server
        .set_task_goal("goal-session", Some("finish safely"))
        .await
        .unwrap();
    let (sink, _receiver) = EventSink::channel();
    crate::turn::run_turn(
        &server,
        TurnStartParams {
            session_id: Some("goal-session".into()),
            message: "start".into(),
            ..Default::default()
        },
        RequestId::Number(1),
        &sink,
    )
    .await
    .unwrap();
    assert_eq!(
        server.tasks.get(&task.id).await.unwrap().unwrap().status,
        joyczl_state::TaskStatus::NeedsAttention
    );
    assert_eq!(mock.received.lock().unwrap().len(), 2);
    let run = server
        .runs
        .list_by_task(&task.id, 1)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(run.status, "waiting");
    let checkpoint = server.checkpoints.get(&run.id).await.unwrap().unwrap();
    assert_eq!(checkpoint.status, "suspended");
    let messages: Vec<joyczl_provider::Message> =
        serde_json::from_str(&checkpoint.messages_json).unwrap();
    assert!(messages
        .last()
        .unwrap()
        .content
        .iter()
        .any(|block| matches!(block, joyczl_provider::ContentBlock::ToolUse { .. })));
}
