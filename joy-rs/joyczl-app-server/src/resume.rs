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
            eprintln!("(joy) checkpoint reconciliation could not read the database (nothing resumes, startup is not blocked): {e}");
            return;
        }
    };
    let max_age = server.settings().resume_max_age_minutes;
    let now = chrono::Utc::now().naive_utc();
    for row in rows {
        if row.run_id.is_some() && row.round_id.is_some() {
            let charged = match crate::task_contract::checkpoint_usage(&row.usage_json) {
                Ok(usage) => crate::task_contract::charge_checkpoint(server, &row, usage).await,
                Err(error) => Err(error),
            };
            if let Err(error) = charged {
                crate::task_contract::stop_recovery(
                    server,
                    sink,
                    &row,
                    &format!("Checkpoint usage could not be charged: {error}"),
                )
                .await;
                continue;
            }
        }
        if let Some(age) = age_minutes(&row.updated_at, now) {
            if max_age > 0 && age > i64::from(max_age) {
                eprintln!(
                    "(joy) reconcile: turn {} stopped before {} (limit {}), not resuming on its own — to carry on, say that sentence again",
                    row.turn_id,
                    humanize(age),
                    humanize(i64::from(max_age)),
                );
                // Marked, not left `open`: otherwise every boot prints the same line again,
                // and the ledger's status column would keep calling it unfinished forever.
                if let Err(error) = server.checkpoints.finish(&row.turn_id, "abandoned").await {
                    eprintln!(
                        "(joy) reconcile: turn {} could not be marked abandoned: {error}",
                        row.turn_id
                    );
                }
                continue;
            }
        }
        eprintln!(
            "(joy) reconcile: found unfinished turn {} ({} rounds done), resuming automatically",
            row.turn_id, row.iterations
        );
        let server = server.clone();
        let sink = sink.clone();
        tokio::spawn(async move { resume_turn(&server, &sink, row, None).await });
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

/// 90 → `1 hour`, 3000 → `2 days`. Rounded down on purpose: "2 days ago" must not read as 1.
pub(crate) fn humanize(minutes: i64) -> String {
    if minutes >= 1_440 {
        format!("{} days", minutes / 1_440)
    } else if minutes >= 60 {
        format!("{} hours", minutes / 60)
    } else {
        format!("{} minutes", minutes.max(0))
    }
}

/// Continue a **suspended** checkpoint on a human's say-so (`task/resume`).
///
/// This is the other half of reconcile: crash recovery resumes what the process left `open`
/// (nobody asked), while a pause is the human's decision and waits for their "continue".
/// No suspended checkpoint for that turn → nothing to continue (the task then waits for
/// its next message instead) — said out loud, not silently skipped.
pub(crate) fn resume_authorized(server: &Server, sink: &EventSink, row: CheckpointRow) {
    let server = server.clone();
    let sink = sink.clone();
    let turn_id = row.turn_id.clone();
    tokio::spawn(async move {
        eprintln!("(joy) task resuming from where it paused: turn {turn_id}");
        resume_turn(&server, &sink, row, None).await;
    });
}

pub(crate) async fn resume_waiting(
    server: &Server,
    sink: &EventSink,
    wait: joyczl_state::waits::JobWait,
    jobs: &[joyczl_state::JobRow],
) -> bool {
    let mut row = match server.checkpoints.get(&wait.turn_id).await {
        Ok(Some(row)) if row.status == "waiting_jobs" => row,
        _ => return false,
    };
    let mut messages: Vec<Message> = match serde_json::from_str(&row.messages_json) {
        Ok(messages) => messages,
        Err(_) => return false,
    };
    if crate::task_recovery::require_known_messages(&messages).is_err() {
        let reason = "Waiting checkpoint has an unknown tool intent; reconcile before resuming";
        let _ = server.tasks.needs_attention(&wait.task_id, reason).await;
        let _ = server.checkpoints.finish(&wait.turn_id, "suspended").await;
        let _ = joyczl_state::waits::JobWaits::new(server.pool.clone())
            .cancel(&wait.task_id)
            .await;
        eprintln!("(joy) {}: {reason}", wait.task_id);
        return false;
    }
    let summary = jobs
        .iter()
        .map(|job| {
            format!(
                "{}: {}, exit {:?}; output {}",
                job.id,
                job.status,
                job.exit_code,
                job.log_path.as_deref().unwrap_or("unavailable")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    messages.push(Message::user_text(format!("The background jobs for this authorized task have finished. Review the actual output and verify the task before claiming completion. Do not relaunch a completed command merely because this turn resumed.\n{summary}")));
    row.messages_json = match serde_json::to_string(&messages) {
        Ok(json) => json,
        Err(_) => return false,
    };
    resume_turn(server, sink, row, Some(crate::wake::Wake::Jobs(wait))).await
}

/// Continue one checkpointed turn and give it the same accounting as a fresh one.
pub(crate) async fn resume_turn(
    server: &Server,
    sink: &EventSink,
    row: CheckpointRow,
    waiting: Option<crate::wake::Wake>,
) -> bool {
    let Ok(messages) = serde_json::from_str::<Vec<Message>>(&row.messages_json) else {
        eprintln!(
            "(joy) the working memory of checkpoint {} could not be read (schema drift?), marked abandoned",
            row.turn_id
        );
        let _ = server.checkpoints.finish(&row.turn_id, "abandoned").await;
        return false;
    };
    if crate::task_recovery::require_known_messages(&messages).is_err() {
        crate::task_contract::stop_recovery(
            server,
            sink,
            &row,
            "Tool outcome unknown after interruption; reconcile before retrying",
        )
        .await;
        return false;
    }
    let mut usage: Usage = match crate::task_contract::checkpoint_usage(&row.usage_json) {
        Ok(usage) => usage,
        Err(error) => {
            crate::task_contract::stop_recovery(server, sink, &row, &error).await;
            return false;
        }
    };
    let resolved: Resolved = match server.resolved() {
        Some(resolved) => resolved,
        None => {
            if row.round_id.is_some() {
                if let Err(error) =
                    crate::task_contract::charge_checkpoint(server, &row, usage).await
                {
                    crate::task_contract::stop_recovery(
                        server,
                        sink,
                        &row,
                        &format!("Checkpoint usage could not be charged: {error}"),
                    )
                    .await;
                    return false;
                }
            }
            eprintln!(
                "(joy) keeping checkpoint {} for now: no model is configured; it resumes at the next start once there is one",
                row.turn_id
            );
            return false;
        }
    };
    let session_id = row.session_id.clone();
    let turn_id = row.turn_id.clone();

    if row.round_id.is_none() {
        if let Some(id) = row.run_id.as_deref() {
            if let Ok(Some(run)) = server.runs.get(id).await {
                if run.rounds > 0 {
                    let reason = "Legacy checkpoint has no round accounting identity; review spend before resuming";
                    let _ = server.tasks.needs_attention(&run.task_id, reason).await;
                    let _ = server.checkpoints.finish(&row.turn_id, "suspended").await;
                    eprintln!("(joy) {}: {reason}", row.turn_id);
                    return false;
                }
            }
        }
    }
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
    let task = match row.run_id.as_deref() {
        Some(id) => match server.runs.get(id).await {
            Ok(Some(run)) => match server.tasks.get(&run.task_id).await {
                Ok(Some(task)) if task.status.is_open() => Some(task),
                _ => {
                    eprintln!("(joy) resume stopped: the task of run {id} is unavailable");
                    return false;
                }
            },
            _ => {
                eprintln!("(joy) resume stopped: run {id} was not found");
                return false;
            }
        },
        None => None,
    };
    let background_lock = match &task {
        Some(task) => match joyczl_state::dispatch::TaskDispatch::new(server.pool.clone())
            .get(&task.id)
            .await
        {
            Ok(Some(_)) => Some(server.session_write_lock("background-task-worker")),
            Ok(None) => None,
            Err(error) => {
                eprintln!("(joy) cannot read recovered task dispatch: {error}");
                return false;
            }
        },
        None => None,
    };
    let _background_guard = match &background_lock {
        Some(lock) => Some(lock.lock().await),
        None => None,
    };
    // Same table as a fresh turn: turn/interrupt can stop a resumed one too. The session gate
    // applies here as well — two checkpoints of one session must not resume side by side.
    // Losing the race leaves this checkpoint `open`; the next boot picks it up again.
    let (interrupt, steering) = match server.begin_turn(&turn_id, &session_id) {
        Ok(pair) => pair,
        Err(holder) => {
            eprintln!(
                "(joy) session {session_id} already has {holder} running; checkpoint {turn_id} is left for the next start"
            );
            return false;
        }
    };
    let _guard = TurnGuard {
        server,
        turn_id: turn_id.clone(),
        session_id: session_id.clone(),
    };

    let control = task
        .as_ref()
        .map(|task| server.session_write_lock(&format!("task-control:{}", task.id)));
    let control_guard = match control.as_ref() {
        Some(lock) => Some(lock.lock().await),
        None => None,
    };
    if let Some(task) = &task {
        match joyczl_state::human_pauses::HumanPauses::new(server.pool.clone())
            .is_paused(&task.id)
            .await
        {
            Ok(false) => (),
            Ok(true) => {
                let _ = server.checkpoints.finish(&turn_id, "suspended").await;
                eprintln!(
                    "(joy) task {} remains paused by the user; automatic recovery was refused",
                    task.id
                );
                crate::task::notify(
                    sink,
                    task,
                    Some("Paused by the user; explicit continuation is required"),
                );
                return false;
            }
            Err(error) => {
                eprintln!("(joy) human pause could not be verified; recovery stopped: {error}");
                return false;
            }
        }
        match server.runs.list_by_task(&task.id, 1).await {
            Ok(runs) if runs.first().map(|run| run.id.as_str()) == row.run_id.as_deref() => (),
            Ok(_) => {
                if let Err(error) = server.checkpoints.finish(&turn_id, "abandoned").await {
                    eprintln!("(joy) stale checkpoint could not be closed: {error}");
                }
                if let Some(id) = &row.run_id {
                    if let Err(error) = server.runs.supersede(id).await {
                        eprintln!("(joy) stale run could not be suspended: {error}");
                    }
                }
                eprintln!(
                    "(joy) checkpoint {turn_id} belongs to an older execution; it will not resume"
                );
                return false;
            }
            Err(error) => {
                eprintln!(
                    "(joy) latest execution could not be verified; recovery stopped: {error}"
                );
                return false;
            }
        }
        match server.tasks.get(&task.id).await {
            Ok(Some(current)) if current.status.is_open() => {}
            _ => {
                eprintln!("(joy) recovered task is no longer open; execution stopped");
                return false;
            }
        }
        let expected = waiting
            .as_ref()
            .map(crate::wake::Wake::checkpoint_status)
            .unwrap_or("open");
        match server.checkpoints.get(&turn_id).await {
            Ok(Some(current)) if current.status == expected => {}
            _ => {
                eprintln!("(joy) recovered checkpoint admission changed; execution stopped");
                return false;
            }
        }
    }
    if let Some(task) = &task {
        let admission = match crate::task_contract::charge_checkpoint(server, &row, usage).await {
            Ok(()) => match server.tasks.get(&task.id).await {
                Ok(Some(current)) => crate::task_contract::admit(server, sink, &current)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.message),
                _ => Err("Recovered task is unavailable".into()),
            },
            Err(error) => Err(format!("Checkpoint usage could not be charged: {error}")),
        };
        if let Err(reason) = admission {
            crate::task_contract::stop_recovery(server, sink, &row, &reason).await;
            return false;
        }
    }
    if let Some(wait) = &waiting {
        match wait.claim(server, &row.messages_json).await {
            Ok(true) => eprintln!(
                "(joy) task {} input dependencies settled; resuming authorized work",
                wait.task_id()
            ),
            Ok(false) => return false,
            Err(error) => {
                eprintln!("(joy) durable wait could not be claimed: {error}");
                return false;
            }
        }
    }
    if let Some(task) = &task {
        if let Err(error) = server.tasks.set_running(&task.id, &turn_id).await {
            eprintln!("(joy) recovered task activation failed; execution stopped: {error}");
            let _ = server.checkpoints.finish(&turn_id, "suspended").await;
            return false;
        }
        if let Ok(Some(row)) = server.tasks.get(&task.id).await {
            crate::task::notify(sink, &row, Some("turn resumed"));
        }
    }
    sink.notification(ServerNotification::TurnResumed(TurnResumedNotification {
        turn_id: turn_id.clone(),
        session_id: session_id.clone(),
        from_iterations: row.iterations,
        reason: "reconcile: crash recovery".to_string(),
    }));

    drop(control_guard);

    // The run comes back with the checkpoint: this is the **same attempt** continuing, not a
    // new one. Completed checkpointed tools are retained; unknown in-flight side effects
    // still require an execution-time operation journal before safe replay can be guaranteed.
    let run_id = row.run_id.clone();
    if let Some(run_id) = &run_id {
        match server.runs.reopen(run_id).await {
            Ok(true) => {}
            outcome => {
                let reason = format!("Cannot reopen checkpoint run {run_id}: {outcome:?}");
                eprintln!("(joy) {reason}");
                if let Some(task) = &task {
                    let _ = server.tasks.needs_attention(&task.id, &reason).await;
                }
                let _ = server.checkpoints.finish(&turn_id, "suspended").await;
                return false;
            }
        }
    }

    let round_no = row.round_no.unwrap_or_else(|| {
        task.as_ref()
            .map(|task| task.rounds_used.max(1))
            .unwrap_or(1)
    });
    let round_id = row
        .round_id
        .clone()
        .unwrap_or_else(|| format!("legacy:{}", row.turn_id));
    let wall_offset_ms = match run_id.as_deref() {
        Some(id) => match server.runs.round_account(id, &round_id).await {
            Ok(account) => {
                // A crash may follow accounting but precede finalization. Its checkpoint
                // predates the final model response; charge any repeated calls as new work.
                usage.input_tokens = usage.input_tokens.max(account.tokens_in);
                usage.output_tokens = usage.output_tokens.max(account.tokens_out);
                account.wall_ms
            }
            Err(e) => {
                crate::task_contract::stop_recovery(
                    server,
                    sink,
                    &row,
                    &format!("Cannot read recovered round accounting: {e}"),
                )
                .await;
                return false;
            }
        },
        None => 0,
    };
    let plan = match &task {
        Some(task) => match joyczl_agent::plan::restore(&server.steps, &task.id).await {
            Ok(plan) => plan,
            Err(error) => {
                crate::task_contract::stop_recovery(
                    server,
                    sink,
                    &row,
                    &format!("Cannot restore task plan: {error}"),
                )
                .await;
                return false;
            }
        },
        None => None,
    };
    let started = std::time::Instant::now();
    let cwd = task
        .as_ref()
        .and_then(|task| task.cwd.as_deref())
        .map(std::path::PathBuf::from);
    let frame = TurnFrame {
        session_id: &session_id,
        turn_id: &turn_id,
        sink,
        interrupt: Some(interrupt.clone()),
        turn_cwd: cwd.clone(),
        route_reason: None,
        resume: Some(joyczl_loop::Resume {
            round_id: round_id.clone(),
            messages,
            completed_iterations: row.iterations,
            usage,
            // A resume after a crash keeps the session cap: the human's `/resume` is not a
            // step grant. (Continuations inside a turn pass their own slice.)
            max_iterations: None,
        }),
        // Same outlet as a fresh turn: every further boundary is durable again.
        checkpoint: Some(crate::turn::checkpoint_outlet(
            &server.checkpoints,
            sink,
            &turn_id,
            &session_id,
            &row.user_message,
            run_id.as_deref(),
            round_no,
        )),
        // A resumed turn is steerable too: whatever the human adds while the crash recovery
        // runs lands in the same transcript.
        steering: Some(steering.clone()),
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
            let driven = crate::turn::driver::drive_task(
                crate::turn::driver::DriveTask {
                    server,
                    resolved: &resolved,
                    task: task.clone(),
                    run_id: run_id.clone(),
                    frame: TurnFrame {
                        session_id: &session_id,
                        turn_id: &turn_id,
                        sink,
                        interrupt: Some(interrupt.clone()),
                        turn_cwd: cwd.clone(),
                        route_reason: None,
                        resume: None,
                        checkpoint: Some(crate::turn::checkpoint_outlet(
                            &server.checkpoints,
                            sink,
                            &turn_id,
                            &session_id,
                            &row.user_message,
                            run_id.as_deref(),
                            round_no,
                        )),
                        steering: Some(steering.clone()),
                    },
                    plan,
                    round_no,
                    max_rounds: task
                        .as_ref()
                        .map(|task| task.max_rounds.max(1))
                        .unwrap_or(1),
                    started,
                    wall_offset_ms,
                },
                full,
            )
            .await;
            let driven = match driven {
                Ok(driven) => driven,
                Err(error) => {
                    eprintln!("(joy) recovered task driver failed: {}", error.message);
                    return false;
                }
            };
            let full = driven.full;
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
                    goal_status: driven.goal_status,
                    goal_rounds: driven.round_no,
                    // A resumed turn continues from the checkpoint: no history was loaded here.
                    history: full.history,
                    started,
                    run_id,
                    decision: task.as_ref().map(|_| driven.decision),
                    // A resume is not a self-continuation: it is the human saying "go on".
                    continues: 0,
                    unfinished: None,
                },
            )
            .await;
            if let Err(e) = outcome {
                eprintln!(
                    "(joy) the wrap-up of the resumed turn failed: {}",
                    e.message
                );
            }
        }
        Err(e) => {
            crate::turn::fail_turn(
                server,
                sink,
                &turn_id,
                run_id.as_deref(),
                task.as_ref().map(|task| task.id.as_str()),
                &e.message,
            )
            .await;
            eprintln!("(joy) resumed turn failed: {}", e.message);
        }
    }
    true
}
