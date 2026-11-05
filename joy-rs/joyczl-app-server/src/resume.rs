//! Startup reconciliation — the crash-recovery half of the long-task engine.
//!
//! A process that died mid-turn leaves its checkpoint `open` in state.db. At startup those
//! turns resume automatically: the human asked for this work by starting the turn, and the
//! goal loop's rule already covers the spirit — continuing is not the model assigning itself
//! work. Finished tools are never re-run; the loop continues from the checkpointed boundary.
//!
//! What is **not** resumed: turns the human interrupted, and turns that failed inside their
//! process (resuming a failed model call fails the same way again) — both are `abandoned`.
//! After a crash there is nothing else `open` can mean, which is the invariant reconcile
//! stands on. New tool executions still pass the approval gate (default-deny) — resuming
//! re-enters approved territory, it never widens it.

use joyczl_protocol::{ServerNotification, TurnResumedNotification, TurnStartedNotification};
use joyczl_provider::{Message, Resolved, Usage};
use joyczl_state::CheckpointRow;

use crate::turn::{finalize_turn, full_turn, FinishedTurn, TurnFrame, TurnGuard};
use crate::{EventSink, Server};

/// Resume every `open` checkpoint. Called once per transport start; each resume runs as its
/// own task, so one broken turn never blocks the others (or the request loop).
pub async fn reconcile(server: &Server, sink: &EventSink) {
    let rows = match server.checkpoints.open_rows().await {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("(joy) checkpoint 对账读不了库（不续跑，也不挡启动）：{e}");
            return;
        }
    };
    for row in rows {
        eprintln!(
            "(joy) 对账：发现未完成的回合 {}（已完成 {} 轮），自动续跑",
            row.turn_id, row.iterations
        );
        let server = server.clone();
        let sink = sink.clone();
        tokio::spawn(async move { resume_turn(&server, &sink, row).await });
    }
}

/// Continue one checkpointed turn and give it the same accounting as a fresh one.
async fn resume_turn(server: &Server, sink: &EventSink, row: CheckpointRow) {
    let resolved: Resolved = match server.resolved() {
        Some(resolved) => resolved,
        None => {
            eprintln!(
                "(joy) checkpoint {} 先留着：模型没配好，配好后下次启动再续",
                row.turn_id
            );
            return;
        }
    };
    let Ok(messages) = serde_json::from_str::<Vec<Message>>(&row.messages_json) else {
        eprintln!(
            "(joy) checkpoint {} 的工作记忆读不出来（schema 漂移？），标 abandoned",
            row.turn_id
        );
        let _ = server.checkpoints.finish(&row.turn_id, "abandoned").await;
        return;
    };
    let usage: Usage = serde_json::from_str(&row.usage_json).unwrap_or_default();
    let session_id = row.session_id.clone();
    let turn_id = row.turn_id.clone();

    // The resumed turn borrows the original turn_id: it **is** the same turn, continued —
    // the ledger rows (trace, chat log) line up under one id.
    sink.notification(ServerNotification::TurnStarted(TurnStartedNotification {
        turn_id: turn_id.clone(),
        session_id: session_id.clone(),
        user_message: row.user_message.clone(),
        ts: chrono::Local::now().to_rfc3339(),
    }));
    sink.notification(ServerNotification::TurnResumed(TurnResumedNotification {
        turn_id: turn_id.clone(),
        session_id: session_id.clone(),
        from_iterations: row.iterations,
        reason: "reconcile: crash recovery".to_string(),
    }));

    // Same table as a fresh turn: turn/interrupt can stop a resumed one too.
    let interrupt = server.register_turn(&turn_id);
    let _guard = TurnGuard {
        server,
        turn_id: turn_id.clone(),
    };

    let frame = TurnFrame {
        session_id: &session_id,
        turn_id: &turn_id,
        sink,
        interrupt: Some(interrupt.clone()),
        turn_cwd: None,
        route_reason: None,
        resume: Some(joyczl_loop::Resume {
            messages,
            completed_iterations: row.iterations,
            usage,
        }),
        // Same outlet as a fresh turn: every further boundary is durable again.
        checkpoint: Some(crate::turn::checkpoint_outlet(
            &server.checkpoints,
            sink,
            &turn_id,
            &session_id,
            &row.user_message,
        )),
    };

    match full_turn(
        server,
        &resolved,
        &row.user_message,
        Vec::new(),
        frame,
        None,
    )
    .await
    {
        Ok(full) => {
            let outcome = finalize_turn(
                server,
                sink,
                FinishedTurn {
                    session_id,
                    turn_id,
                    user_message: &row.user_message,
                    resolved: &resolved,
                    graph: None,
                    gate: full.gate,
                    result: full.result,
                    route_reason: None,
                    retries: 0,
                    goal_status: None,
                    goal_rounds: 0,
                    started: std::time::Instant::now(),
                },
            )
            .await;
            if let Err(e) = outcome {
                eprintln!("(joy) 续跑回合的收尾失败：{}", e.message);
            }
        }
        Err(e) => {
            let _ = server.checkpoints.finish(&turn_id, "abandoned").await;
            eprintln!("(joy) 续跑失败，checkpoint 已标 abandoned：{}", e.message);
        }
    }
}
