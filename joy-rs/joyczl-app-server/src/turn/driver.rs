//! Shared task driver for fresh turns and recovered checkpoints.
use super::history::{judge_transcript, notify_goal};
use super::plan::revise_plan;
use super::{
    account_round, budget_overrun, checkpoint_outlet, fail_turn, full_turn, observe_round,
    FullTurn, RoundCtx, TaskDecision, TurnFrame,
};
use crate::Server;
use joyczl_protocol::ErrorObject;
use joyczl_provider::Resolved;
use joyczl_state::TaskRow;

pub(crate) struct DriveTask<'a> {
    pub server: &'a Server,
    pub resolved: &'a Resolved,
    pub task: Option<TaskRow>,
    pub run_id: Option<String>,
    pub frame: TurnFrame<'a>,
    pub plan: Option<joyczl_agent::plan::Plan>,
    pub round_no: i32,
    pub max_rounds: i32,
    pub started: std::time::Instant,
    pub wall_offset_ms: i64,
}

pub(crate) struct DrivenTask {
    pub full: FullTurn,
    pub decision: TaskDecision,
    pub goal_status: Option<String>,
    pub round_no: i32,
}

pub(crate) async fn drive_task(
    ctx: DriveTask<'_>,
    mut full: FullTurn,
) -> Result<DrivenTask, ErrorObject> {
    let DriveTask {
        server,
        resolved,
        task,
        run_id,
        frame,
        mut plan,
        mut round_no,
        max_rounds,
        started,
        mut wall_offset_ms,
    } = ctx;
    let TurnFrame {
        session_id,
        turn_id,
        sink,
        turn_cwd,
        ..
    } = frame.clone();
    let (session_id, turn_id) = (session_id.to_string(), turn_id.to_string());
    let task_id = task.as_ref().map(|task| task.id.clone());
    let goal_condition = task.as_ref().and_then(|task| task.goal_condition.clone());
    let acceptance = match task
        .as_ref()
        .map(|task| joyczl_agent::verify::parse_checked(&task.acceptance_json))
        .transpose()
    {
        Ok(acceptance) => acceptance.unwrap_or_default(),
        Err(reason) => {
            return Ok(DrivenTask {
                full,
                decision: TaskDecision::NeedsAttention(reason),
                goal_status: Some("invalid-contract".into()),
                round_no,
            })
        }
    };
    let verify_root = turn_cwd.clone();
    let mut observation = observe_round(&full.result, &turn_id, turn_cwd.as_deref());
    if let Some(condition) = &goal_condition {
        eprintln!("(joy) goal loop starts: {condition} (max {max_rounds} rounds)");
    }
    let mut round_started = started;
    // Each loop round has a stable identity; recovered slices reuse its local operation keys.

    // The loop's two products: the verdict (what the task becomes) and the goal status (what
    // the trace and the notifications say happened). Both are returned by `break`, so there is
    // no "maybe nothing was decided" state to test for afterwards.
    let (decision, goal_status) = loop {
        // Rounds live on the row (written just below); the notification quotes the same number.
        let goal_rounds = round_no;
        if let Some(id) = &task_id {
            if let Err(e) = server.tasks.record_round(id, round_no).await {
                break (
                    TaskDecision::NeedsAttention(format!("Cannot record task round: {e}")),
                    Some("accounting-failed".into()),
                );
            }
        }

        // ---- Account the round: steps, artifacts, spend — then measure it.
        let checks = match account_round(
            server,
            RoundCtx {
                task: task.as_ref(),
                run_id: run_id.as_deref(),
                acceptance: &acceptance,
                verify_root: verify_root.clone(),
                started: round_started,
                wall_offset_ms,
            },
            &observation,
            &full.result,
        )
        .await
        {
            Ok(checks) => checks,
            Err(reason) => {
                eprintln!("(joy) {reason}; task stopped for review");
                break (
                    TaskDecision::NeedsAttention(reason),
                    Some("accounting-failed".to_string()),
                );
            }
        };

        // ---- 0. A stop is the human's, and it ends the loop here.
        //
        // Without this the goal loop happily spends another round after `turn/interrupt`:
        // the interrupted round's acceptance fails, "keep trying" kicks in, and the user's stop
        // only takes effect when the rounds run out. The turn tail already turns an interrupted
        // result into "suspended at the checkpoint"; the decision below never wins over that.
        if full.result.interrupted {
            eprintln!("(joy) interrupted; the goal loop stops at this round");
            break (
                TaskDecision::NeedsAttention(
                    "Interrupted by the user; the task stays where it is".to_string(),
                ),
                Some("interrupted".to_string()),
            );
        }

        // ---- 1. Budget, before anything can spend more.
        if let Some(id) = &task_id {
            match server.tasks.get(id).await {
                Ok(Some(fresh)) => {
                    if let Some(over) = budget_overrun(&fresh) {
                        eprintln!("(joy) task {id} stopped on its budget: {over}");
                        if let Err(e) = server
                            .events
                            .append(id, run_id.as_deref(), "budget", Some(&over))
                            .await
                        {
                            eprintln!("(joy) budget event not recorded: {e}");
                        }
                        break (
                            TaskDecision::NeedsAttention(over),
                            Some("over-budget".to_string()),
                        );
                    }
                }
                Ok(None) => {
                    break (
                        TaskDecision::NeedsAttention("Task disappeared during execution".into()),
                        Some("task-unavailable".into()),
                    )
                }
                Err(e) => {
                    break (
                        TaskDecision::NeedsAttention(format!("Cannot verify task budget: {e}")),
                        Some("budget-unavailable".into()),
                    )
                }
            }
        }

        if full.result.stop_reason == joyczl_provider::StopReason::UncertainEffect {
            break (
                TaskDecision::NeedsAttention(
                    "Tool outcome unknown; reconcile external state before continuing".into(),
                ),
                Some("unknown-effect".into()),
            );
        }

        // An asynchronous command is unfinished work, not a failed acceptance condition
        // to repair by launching the same command again.
        if let Some(task) = &task {
            match joyczl_state::Jobs::new(server.pool.clone())
                .for_task(&task.id)
                .await
            {
                Ok(jobs) if jobs.iter().any(|job| job.status == "running") => {
                    eprintln!("(joy) task {} is waiting for background jobs", task.id);
                    break (
                        TaskDecision::WaitingForJobs(
                            jobs.into_iter()
                                .filter(|job| job.status == "running")
                                .map(|job| job.id)
                                .collect(),
                        ),
                        Some("waiting-jobs".into()),
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    break (
                        TaskDecision::NeedsAttention(format!("Cannot verify background jobs: {e}")),
                        Some("jobs-unavailable".into()),
                    )
                }
            }
        }

        if matches!(
            full.result.stop_reason,
            joyczl_provider::StopReason::MaxTokens | joyczl_provider::StopReason::IterationLimit
        ) {
            let reason = match full.result.stop_reason {
                joyczl_provider::StopReason::MaxTokens => {
                    "Model output reached its token limit; task completion is not verified"
                }
                _ => "The loop reached its step limit; task completion is not verified",
            };
            break (
                TaskDecision::NeedsAttention(reason.into()),
                Some("response-limit".into()),
            );
        }

        if let Some(task) = &task {
            let inputs = joyczl_state::external_waits::ExternalWaits::new(server.pool.clone());
            match inputs.get(&task.id).await {
                Ok(Some(input))
                    if input.event_id.is_none()
                        || run_id.is_none()
                        || input.consumed_run_id.as_deref() != run_id.as_deref() =>
                {
                    match inputs.expired(&task.id).await {
                        Ok(false) => {
                            break (
                                TaskDecision::WaitingForInput(input.condition_id),
                                Some("waiting-input".into()),
                            )
                        }
                        Ok(true) => {
                            break (
                                TaskDecision::NeedsAttention(
                                    "Expected input did not arrive before its deadline".into(),
                                ),
                                Some("input-expired".into()),
                            )
                        }
                        Err(error) => {
                            break (
                                TaskDecision::NeedsAttention(format!(
                                    "Cannot verify task input deadline: {error}"
                                )),
                                Some("input-unavailable".into()),
                            )
                        }
                    }
                }
                Ok(_) => (),
                Err(error) => {
                    break (
                        TaskDecision::NeedsAttention(format!(
                            "Cannot verify task input condition: {error}"
                        )),
                        Some("input-unavailable".into()),
                    )
                }
            }
        }

        // ---- 2. Acceptance: a failing condition is a fact, and facts end rounds.
        if let Some(reason) = joyczl_agent::verify::failure_summary(&checks) {
            let who = task_id.as_deref().unwrap_or("(no task)");
            if round_no >= max_rounds {
                let reason = format!("{reason} (after {round_no} rounds it still has not passed)");
                eprintln!("(joy) task {who} stopped at acceptance: {reason}");
                break (
                    TaskDecision::NeedsAttention(reason),
                    Some("verify-failed".to_string()),
                );
            }
            eprintln!("(joy) task {who} failed acceptance on round {round_no}: {reason}");
            round_no += 1;
            round_started = std::time::Instant::now();
            wall_offset_ms = 0;
            plan = revise_plan(server, resolved, task.as_ref(), &acceptance, plan, &reason).await;
            // Feed the measurement back — not the judge's opinion, and not "try harder".
            let mut correction = format!(
"Acceptance did not pass: {reason}\nKeep working until it is true (the file really written, the change really in place); do not merely claim it is done."
            );
            if let Some(current) = &plan {
                correction.push_str("\n\n");
                correction.push_str(&current.brief());
            }
            let next = full_turn(
                server,
                resolved,
                &correction,
                Vec::new(),
                TurnFrame {
                    resume: None,
                    checkpoint: Some(checkpoint_outlet(
                        &server.checkpoints,
                        sink,
                        &turn_id,
                        &session_id,
                        &correction,
                        run_id.as_deref(),
                        round_no,
                    )),
                    ..frame.clone()
                },
                None,
            )
            .await;
            match next {
                Ok(next) => {
                    observation = observe_round(&next.result, &turn_id, turn_cwd.as_deref());
                    full = next;
                    continue;
                }
                Err(e) => {
                    fail_turn(
                        server,
                        sink,
                        &turn_id,
                        run_id.as_deref(),
                        task_id.as_deref(),
                        &e.message,
                    )
                    .await;
                    return Err(e);
                }
            }
        }

        // ---- 3. A contradiction is never "done": the tool said it wrote, the file is not there.
        if let Some(contradiction) = observation.contradictions().first() {
            eprintln!(
                "(joy) task {} stopped on a contradiction: {contradiction}",
                task_id.as_deref().unwrap_or("(no task)")
            );
            break (
                TaskDecision::NeedsAttention(contradiction.clone()),
                Some("contradiction".to_string()),
            );
        }

        // ---- 4. The judge, only now, and only when a condition was declared.
        let Some(condition) = &goal_condition else {
            // Nothing left to decide: acceptance conditions (if any) just passed; a task with
            // none is a single-turn task — done when the turn is, exactly as before 0.14.
            break (
                TaskDecision::Done,
                if checks.is_empty() {
                    None
                } else {
                    Some("verified".to_string())
                },
            );
        };

        let transcript = judge_transcript(server, &session_id, &full.result.reply).await;
        let judgement = match crate::goal::judge(resolved, condition, &transcript).await {
            Ok(judgement) => judgement,
            Err(why) => {
                // The judge broke down: stop continuing, **keep the goal** (don't fake success).
                notify_goal(sink, &turn_id, goal_rounds, max_rounds, "blocked", &why);
                eprintln!("(joy) goal loop stopped: {why}");
                // A broken judge is not "the goal is impossible" — a human should look.
                break (
                    TaskDecision::NeedsAttention(format!("Judge failure: {why}")),
                    Some("blocked".to_string()),
                );
            }
        };

        if judgement.ok {
            notify_goal(
                sink,
                &turn_id,
                goal_rounds,
                max_rounds,
                "satisfied",
                &judgement.reason,
            );
            break (TaskDecision::Done, Some("satisfied".to_string()));
        }
        if judgement.impossible {
            notify_goal(
                sink,
                &turn_id,
                goal_rounds,
                max_rounds,
                "impossible",
                &judgement.reason,
            );
            break (
                TaskDecision::Failed(judgement.reason),
                Some("impossible".to_string()),
            );
        }
        if round_no >= max_rounds {
            // Cap reached: **say honestly it is not done** — don't fake completion, don't clear the goal.
            let reason = format!(
                "After {max_rounds} more rounds the goal is still unmet: {}",
                judgement.reason
            );
            notify_goal(
                sink,
                &turn_id,
                goal_rounds,
                max_rounds,
                "round-limit",
                &reason,
            );
            break (
                TaskDecision::Failed(reason),
                Some("round-limit".to_string()),
            );
        }

        notify_goal(
            sink,
            &turn_id,
            goal_rounds,
            max_rounds,
            "continuing",
            &judgement.reason,
        );
        eprintln!(
            "(joy) goal loop round {goal_rounds} of {max_rounds}: {}",
            judgement.reason
        );
        // Feed the reason back as the next user message and run another round **in the same session**.
        round_no += 1;
        round_started = std::time::Instant::now();
        wall_offset_ms = 0;
        let mut continuation = judgement.reason.clone();
        if let Some(current) = &plan {
            continuation.push_str("\n\n");
            continuation.push_str(&current.brief());
        }
        let next = full_turn(
            server,
            resolved,
            &continuation,
            Vec::new(),
            TurnFrame {
                resume: None,
                checkpoint: Some(checkpoint_outlet(
                    &server.checkpoints,
                    sink,
                    &turn_id,
                    &session_id,
                    &continuation,
                    run_id.as_deref(),
                    round_no,
                )),
                ..frame.clone()
            },
            None,
        )
        .await;
        match next {
            Ok(next) => {
                observation = observe_round(&next.result, &turn_id, turn_cwd.as_deref());
                full = next;
            }
            Err(e) => {
                fail_turn(
                    server,
                    sink,
                    &turn_id,
                    run_id.as_deref(),
                    task_id.as_deref(),
                    &e.message,
                )
                .await;
                return Err(e);
            }
        }
    };

    Ok(DrivenTask {
        full,
        decision,
        goal_status,
        round_no,
    })
}
