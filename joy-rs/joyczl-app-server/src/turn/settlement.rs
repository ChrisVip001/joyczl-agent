//! Settlement stage of the turn lifecycle.

use super::{FinishedTurn, TaskDecision};
use crate::{EventSink, Server};
use joyczl_protocol::{codes, ErrorObject, ServerNotification, TurnSuspendedNotification};
use joyczl_state::{RunStatus, TaskRow, TaskStatus};

pub(super) struct Settlement {
    pub carried_task: Option<TaskRow>,
    pub waiting_for_jobs: bool,
    pub input_wait_committed: bool,
}

pub(super) async fn settle_task(
    server: &Server,
    sink: &EventSink,
    t: &mut FinishedTurn<'_>,
) -> Result<Settlement, ErrorObject> {
    let decision = t.decision.take();
    let FinishedTurn {
        session_id,
        turn_id,
        result,
        run_id,
        goal_status,
        goal_rounds,
        ..
    } = &*t;
    // ---- Checkpoint bookkeeping: the turn reached an end **in this process**. From here on,
    // `open` can only mean "the process died mid-turn" — the invariant the startup reconcile
    // relies on. Two kinds of interruption must not be conflated:
    //   * a stop (turn/interrupt): `abandoned`, never auto-resumed;
    //   * a pause (task/suspend): `suspended`, kept resumable — `task/resume` continues it.
    let paused = server.take_suspending(turn_id);
    let waiting_for_jobs =
        !paused && !result.interrupted && matches!(decision, Some(TaskDecision::WaitingForJobs(_)));
    let waiting_for_input = !paused
        && !result.interrupted
        && matches!(decision, Some(TaskDecision::WaitingForInput(_)));
    let mut input_wait_committed = false;
    let (final_status, suspended_reason) = if paused {
        (
            "suspended",
            Some("Paused; it can resume from the checkpoint"),
        )
    } else if result.interrupted {
        ("abandoned", Some("interrupted by user"))
    } else if result.stop_reason == joyczl_provider::StopReason::UncertainEffect {
        (
            "suspended",
            Some("Tool outcome unknown; reconcile before continuing"),
        )
    } else if waiting_for_jobs {
        ("waiting_jobs", Some(joyczl_state::waits::JOB_WAIT_REASON))
    } else if waiting_for_input {
        (
            "waiting_input",
            Some(joyczl_state::external_waits::INPUT_WAIT_REASON),
        )
    } else {
        ("done", None)
    };
    if let Err(e) = server.checkpoints.finish(turn_id, final_status).await {
        eprintln!("(joy) checkpoint close-out not recorded (the turn itself is unaffected): {e}");
    }
    if let Some(reason) = suspended_reason {
        sink.notification(ServerNotification::TurnSuspended(
            TurnSuspendedNotification {
                turn_id: turn_id.clone(),
                session_id: session_id.clone(),
                reason: reason.to_string(),
            },
        ));
    }

    // ---- Task bookkeeping: the same single tail (a resumed turn goes through here too).
    //
    // Precedence is deliberate: a human's pause or stop wins over anything a machine concluded.
    // Then the turn's own decision — which came from measurements, never from the reply text.
    let carried_task = server.tasks.by_turn(turn_id).await.ok().flatten();
    if let Some(task) = &carried_task {
        let control = server.session_write_lock(&format!("task-control:{}", task.id));
        let _control = control.lock().await;
        let latest = server.tasks.get(&task.id).await.ok().flatten();
        let task = latest.as_ref().unwrap_or(task);
        let human_paused = joyczl_state::human_pauses::HumanPauses::new(server.pool.clone())
            .is_paused(&task.id)
            .await
            .map_err(|error| ErrorObject {
                code: codes::INTERNAL_ERROR,
                message: format!("Cannot verify human task control: {error}"),
                data: None,
            })?;
        let paused = paused
            || human_paused
            || server.take_suspending(turn_id)
            || (task.status == TaskStatus::Suspended
                && task.suspend_reason.as_deref() != Some(joyczl_state::waits::JOB_WAIT_REASON));
        let (verdict_reason, run_status) = if task.status == TaskStatus::Cancelled {
            let _ = server.checkpoints.finish(turn_id, "abandoned").await;
            (
                Some("Cancelled by the user".to_string()),
                RunStatus::Cancelled,
            )
        } else if paused {
            let _ = server.checkpoints.finish(turn_id, "suspended").await;
            let reason = "Paused; it can resume from the checkpoint";
            let _ = server.tasks.suspend(&task.id, Some(reason)).await;
            (Some(reason.to_string()), RunStatus::Suspended)
        } else if result.interrupted {
            let reason = "Interrupted by the user; the task stays where it is";
            let _ = server.tasks.suspend(&task.id, Some(reason)).await;
            (Some(reason.to_string()), RunStatus::Suspended)
        } else {
            match decision {
                Some(TaskDecision::Done) => {
                    crate::completion_delivery::finish_task(
                        server,
                        &task.id,
                        TaskStatus::Done,
                        None,
                        turn_id,
                        run_id.as_deref(),
                    )
                    .await?;
                    (None, RunStatus::Completed)
                }
                Some(TaskDecision::WaitingForInput(condition_id)) => {
                    let registered = async {
                        let id = run_id
                            .as_deref()
                            .ok_or_else(|| anyhow::anyhow!("Task has no execution identity"))?;
                        let wait = joyczl_state::external_waits::InputWait {
                            task_id: task.id.clone(),
                            condition_id,
                            run_id: id.into(),
                            turn_id: turn_id.clone(),
                            session_id: session_id.clone(),
                            state: "waiting".into(),
                        };
                        crate::task::recovery::require_known_messages(&result.messages)
                            .map_err(|error| anyhow::anyhow!(error.message))?;
                        let messages = serde_json::to_string(&result.messages)?;
                        let usage = serde_json::to_string(&result.usage)?;
                        joyczl_state::external_waits::ExternalWaits::new(server.pool.clone())
                            .register(
                                &wait,
                                joyczl_state::external_waits::WaitSnapshot {
                                    messages_json: &messages,
                                    usage_json: &usage,
                                    iterations: result.iterations,
                                    round_id: &result.round_id,
                                    round_no: *goal_rounds,
                                },
                            )
                            .await
                    }
                    .await;
                    match registered {
                        Ok(()) => {
                            input_wait_committed = true;
                            (
                                Some(joyczl_state::external_waits::INPUT_WAIT_REASON.into()),
                                RunStatus::Waiting,
                            )
                        }
                        Err(error) => {
                            let reason = format!("Cannot persist external input wait: {error}");
                            eprintln!("(joy) {reason}");
                            let _ = server.tasks.needs_attention(&task.id, &reason).await;
                            let _ = server.checkpoints.finish(turn_id, "suspended").await;
                            (Some(reason), RunStatus::Waiting)
                        }
                    }
                }
                Some(TaskDecision::WaitingForJobs(job_ids)) => {
                    let waits = joyczl_state::waits::JobWaits::new(server.pool.clone());
                    let registered = match run_id.as_deref() {
                        Some(id) => {
                            waits
                                .register(&task.id, id, turn_id, session_id, &job_ids)
                                .await
                        }
                        None => Err(anyhow::anyhow!("task has no execution identity")),
                    };
                    match registered {
                        Ok(()) => (
                            Some(joyczl_state::waits::JOB_WAIT_REASON.to_string()),
                            RunStatus::Waiting,
                        ),
                        Err(e) => {
                            let reason = format!("Cannot persist background-job wait: {e}");
                            eprintln!("(joy) {reason}");
                            let _ = server.tasks.needs_attention(&task.id, &reason).await;
                            let _ = server.checkpoints.finish(turn_id, "suspended").await;
                            (Some(reason), RunStatus::Waiting)
                        }
                    }
                }
                Some(TaskDecision::NeedsAttention(reason)) => {
                    eprintln!("(joy) task {} needs your call: {reason}", task.id);
                    let _ = server.tasks.needs_attention(&task.id, &reason).await;
                    (Some(reason), RunStatus::Waiting)
                }
                Some(TaskDecision::Failed(reason)) => {
                    crate::completion_delivery::finish_task(
                        server,
                        &task.id,
                        TaskStatus::Failed,
                        Some(&reason),
                        turn_id,
                        run_id.as_deref(),
                    )
                    .await?;
                    (Some(reason), RunStatus::Failed)
                }
                // Nobody decided (a resumed turn): fall back to what the goal loop said, and to
                // the pre-0.14 rule that a task without a condition is done when its turn is.
                None => match goal_status.as_deref() {
                    Some("satisfied") => {
                        crate::completion_delivery::finish_task(
                            server,
                            &task.id,
                            TaskStatus::Done,
                            None,
                            turn_id,
                            run_id.as_deref(),
                        )
                        .await?;
                        (None, RunStatus::Completed)
                    }
                    Some(status @ ("impossible" | "round-limit")) => {
                        crate::completion_delivery::finish_task(
                            server,
                            &task.id,
                            TaskStatus::Failed,
                            Some(status),
                            turn_id,
                            run_id.as_deref(),
                        )
                        .await?;
                        (Some(status.to_string()), RunStatus::Failed)
                    }
                    Some("blocked") => {
                        let reason = "Judge failure; stopped here";
                        let _ = server.tasks.needs_attention(&task.id, reason).await;
                        (Some(reason.to_string()), RunStatus::Waiting)
                    }
                    Some(_) => {
                        let _ = server.tasks.clear_turn(&task.id).await;
                        (None, RunStatus::Running)
                    }
                    None => {
                        if task.goal_condition.is_some() {
                            let reason =
                                "The goal did not advance this round; waiting for the next message";
                            let _ = server.tasks.suspend(&task.id, Some(reason)).await;
                            (Some(reason.to_string()), RunStatus::Suspended)
                        } else {
                            crate::completion_delivery::finish_task(
                                server,
                                &task.id,
                                TaskStatus::Done,
                                None,
                                turn_id,
                                run_id.as_deref(),
                            )
                            .await?;
                            (None, RunStatus::Completed)
                        }
                    }
                },
            }
        };
        // The run's account closes with the turn — one execution, one row.
        if let Some(run_id) = &run_id {
            if let Err(e) = server
                .runs
                .close(run_id, run_status, verdict_reason.as_deref())
                .await
            {
                eprintln!("(joy) run record not closed out: {e}");
            }
        }
        if let Ok(Some(row)) = server.tasks.get(&task.id).await {
            crate::task::notify(sink, &row, verdict_reason.as_deref());
        }
    } else if let Some(run_id) = &run_id {
        // A run with no task row behind it (deleted mid-turn): close it rather than leave it open.
        let _ = server
            .runs
            .close(run_id, RunStatus::Failed, Some("the task row is gone"))
            .await;
    }

    Ok(Settlement {
        carried_task,
        waiting_for_jobs,
        input_wait_committed,
    })
}
