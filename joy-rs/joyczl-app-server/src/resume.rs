//! Startup reconciliation — the crash-recovery half of the long-task engine.
//!
//! A process that died mid-turn leaves its checkpoint `open` in state.db. At startup those
//! turns resume automatically: the human asked for this work by starting the turn, and the
//! goal loop's rule already covers the spirit — continuing is not the model assigning itself
//! work. Finished tools are never re-run; the loop continues from the checkpointed boundary.
//!
//! What is **not** resumed: turns the human interrupted, turns that failed inside their
//! process (resuming a failed model call fails the same way again), and turns older than
//! `JOY_RESUME_MAX_AGE` (default one hour) — all three are `abandoned`, each with one line
//! on stderr. New tool executions still pass the approval gate (default-deny) — resuming
//! re-enters approved territory, it never widens it.
//!
//! The age limit is not caution for its own sake. A checkpoint exists for crash recovery,
//! so its honest window is seconds to minutes; a turn left `open` for two days is not "the
//! same turn continued". "Give me today's tech news" answered two days later answers a
//! question nobody is asking any more, and it spends a real model call plus a real fetch to
//! do it. Asking again costs one sentence.

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
    let max_age = server.settings().resume_max_age_minutes;
    let now = chrono::Utc::now().naive_utc();
    for row in rows {
        if let Some(age) = age_minutes(&row.updated_at, now) {
            if max_age > 0 && age > i64::from(max_age) {
                eprintln!(
                    "(joy) 对账：回合 {} 停在 {}前（上限 {}），没有自动续跑 —— 要接着做，把那句话再说一遍",
                    row.turn_id,
                    humanize(age),
                    humanize(i64::from(max_age)),
                );
                // Marked, not left `open`: otherwise every boot prints the same line again,
                // and the ledger's status column would keep calling it unfinished forever.
                if let Err(error) = server.checkpoints.finish(&row.turn_id, "abandoned").await {
                    eprintln!("(joy) 对账：回合 {} 没能标 abandoned：{error}", row.turn_id);
                }
                continue;
            }
        }
        eprintln!(
            "(joy) 对账：发现未完成的回合 {}（已完成 {} 轮），自动续跑",
            row.turn_id, row.iterations
        );
        let server = server.clone();
        let sink = sink.clone();
        tokio::spawn(async move { resume_turn(&server, &sink, row).await });
    }
}

/// How long ago the checkpoint was written, in minutes — `None` when the stamp is unreadable.
///
/// `updated_at` comes from SQLite's `datetime('now')`: UTC, second resolution, no zone
/// marker. A stamp we cannot parse is **not** treated as stale: refusing to resume costs
/// the human work they asked for, while resuming is exactly what the old rule did. Unknown
/// is a reason to be loud on stderr, not a reason to throw a turn away.
pub(crate) fn age_minutes(updated_at: &str, now: chrono::NaiveDateTime) -> Option<i64> {
    let at = chrono::NaiveDateTime::parse_from_str(updated_at.trim(), "%Y-%m-%d %H:%M:%S").ok()?;
    Some((now - at).num_minutes())
}

/// 90 → `1 小时`, 3000 → `2 天`. Rounded down on purpose: "2 天前" must not read as 1.
pub(crate) fn humanize(minutes: i64) -> String {
    if minutes >= 1_440 {
        format!("{} 天", minutes / 1_440)
    } else if minutes >= 60 {
        format!("{} 小时", minutes / 60)
    } else {
        format!("{} 分钟", minutes.max(0))
    }
}

/// Continue a **suspended** checkpoint on a human's say-so (`task/resume`).
///
/// This is the other half of reconcile: crash recovery resumes what the process left `open`
/// (nobody asked), while a pause is the human's decision and waits for their "continue".
/// No suspended checkpoint for that turn → nothing to continue (the task then waits for
/// its next message instead) — said out loud, not silently skipped.
pub(crate) async fn resume_suspended(server: &Server, sink: &EventSink, turn_id: &str) {
    let row = match server.checkpoints.get(turn_id).await {
        Ok(Some(row)) if row.status == "suspended" => row,
        Ok(_) => {
            eprintln!("(joy) 回合 {turn_id} 没有可续的暂停点，等下一句话再推进");
            return;
        }
        Err(e) => {
            eprintln!("(joy) 读不了暂停中的 checkpoint（{turn_id}）：{e}");
            return;
        }
    };
    // Back to `open` **before** the loop runs: every further boundary checkpoints normally,
    // and if this process dies mid-resume the next boot continues it like any other crash.
    if let Err(e) = server.checkpoints.reopen(turn_id).await {
        eprintln!("(joy) 续跑前没能把 checkpoint 改回 open：{e}");
        return;
    }
    let server = server.clone();
    let sink = sink.clone();
    let turn_id = turn_id.to_string();
    tokio::spawn(async move {
        eprintln!("(joy) 任务从暂停处继续：回合 {turn_id}");
        resume_turn(&server, &sink, row).await;
    });
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
    // The task carrying this turn comes back to life with it: boot reconciled a `running` row
    // to `suspended` (the process had died), and continuing is what flips it back.
    if let Ok(Some(task)) = server.tasks.active(&session_id).await {
        if server.tasks.set_running(&task.id, &turn_id).await.is_ok() {
            if let Ok(Some(row)) = server.tasks.get(&task.id).await {
                crate::task::notify(sink, &row, Some("回合继续"));
            }
        }
    }
    sink.notification(ServerNotification::TurnResumed(TurnResumedNotification {
        turn_id: turn_id.clone(),
        session_id: session_id.clone(),
        from_iterations: row.iterations,
        reason: "reconcile: crash recovery".to_string(),
    }));

    // Same table as a fresh turn: turn/interrupt can stop a resumed one too. The session gate
    // applies here as well — two checkpoints of one session must not resume side by side.
    // Losing the race leaves this checkpoint `open`; the next boot picks it up again.
    let (interrupt, steering) = match server.begin_turn(&turn_id, &session_id) {
        Ok(pair) => pair,
        Err(holder) => {
            eprintln!(
                "(joy) 会话 {session_id} 里已经有 {holder} 在跑，checkpoint {turn_id} 留到下次启动"
            );
            return;
        }
    };
    let _guard = TurnGuard {
        server,
        turn_id: turn_id.clone(),
        session_id: session_id.clone(),
    };

    // The run comes back with the checkpoint: this is the **same attempt** continuing, not a
    // new one. Its steps left `running` were already settled to `waiting` at boot — nothing is
    // replayed, and unfinished side effects wait for a human.
    let run_id = row.run_id.clone();
    if let Some(run_id) = &run_id {
        match server.runs.get(run_id).await {
            Ok(Some(run)) => {
                if let Err(e) = server
                    .runs
                    .start(
                        &run.id,
                        &run.task_id,
                        run.session_id.as_deref(),
                        Some(&turn_id),
                    )
                    .await
                {
                    eprintln!("(joy) 续跑时执行记录没恢复成 running：{e}");
                }
            }
            Ok(None) => eprintln!("(joy) 续跑：checkpoint 指的 run {run_id} 已经不在台账里"),
            Err(e) => eprintln!("(joy) 续跑：读不到 run {run_id}（{e}）"),
        }
    }

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
            run_id.as_deref(),
        )),
        // A resumed turn is steerable too: whatever the human adds while the crash recovery
        // runs lands in the same transcript.
        steering: Some(steering),
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
                    run_id,
                    // A resumed turn decides nothing about acceptance: the next human turn
                    // measures again with fresh facts.
                    decision: None,
                },
            )
            .await;
            if let Err(e) = outcome {
                eprintln!("(joy) 续跑回合的收尾失败：{}", e.message);
            }
        }
        Err(e) => {
            let _ = server.checkpoints.finish(&turn_id, "abandoned").await;
            if let Some(run_id) = &run_id {
                let _ = server
                    .runs
                    .close(run_id, joyczl_state::RunStatus::Failed, Some(&e.message))
                    .await;
            }
            eprintln!("(joy) 续跑失败，checkpoint 已标 abandoned：{}", e.message);
        }
    }
}
