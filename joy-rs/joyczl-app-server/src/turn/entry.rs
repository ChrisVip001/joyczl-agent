//! Entry stage of the turn lifecycle.

use chrono::Local;
use joyczl_protocol::{
    codes, ConsolidationCompletedNotification, ErrorObject, JsonRpcMessage, JsonRpcResponse,
    RequestId, ServerNotification, TurnStartParams, TurnStartResponse, TurnStartedNotification,
    JSONRPC_VERSION,
};
use joyczl_provider::Resolved;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use super::{
    checkpoint_outlet, continue_turn, driver, fail_turn, finalize_turn, full_turn, graph_route,
    load_plan, new_turn_id, open_run, resolve_attachments, resolve_turn_cwd, retry_outlet,
    session_cwd, FinishedTurn, FullTurn, TurnFrame, TurnGuard,
};
use crate::{route, EventSink, Server};

pub async fn run_turn(
    server: &Server,
    params: TurnStartParams,
    id: RequestId,
    sink: &EventSink,
) -> Result<(), ErrorObject> {
    run_turn_inner(server, params, id, sink, None).await
}

pub(crate) async fn run_turn_inner(
    server: &Server,
    params: TurnStartParams,
    id: RequestId,
    sink: &EventSink,
    dispatch: Option<joyczl_state::dispatch::DispatchRow>,
) -> Result<(), ErrorObject> {
    let started = std::time::Instant::now();

    let session_id = params
        .session_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "default".to_string());

    // ---- Memory distilled in the background since the last turn is reported here.
    //
    // The worker does its work off the clock, so this is the moment the client hears about
    // it: the same `consolidationCompleted` notification, one turn later than before, at
    // the point where the user can actually act on it ("remembered 3 new facts").
    if let Some(worker) = server.memory_worker.as_ref() {
        let pending = worker.take_pending();
        if pending > 0 {
            sink.notification(ServerNotification::ConsolidationCompleted(
                ConsolidationCompletedNotification { new_facts: pending },
            ));
        }
    }

    // ---- Long task: if the session has an active one, this turn carries it. The heartbeat
    // (`current_turn_id`) is what `task/suspend`, `task/resume` and the boot reconcile hang on.
    // Read before model resolution: a task may **declare its tier**, and that pins this turn's model.
    let task = if let Some(dispatch) = &dispatch {
        server.tasks.get(&dispatch.task_id).await
    } else {
        server.tasks.active(&session_id).await
    }
    .map_err(|e| ErrorObject {
        code: codes::INTERNAL_ERROR,
        message: format!("Cannot read task state: {e}"),
        data: None,
    })?;
    if dispatch.is_some() && task.is_none() {
        return Err(ErrorObject {
            code: codes::INVALID_PARAMS,
            message: "Dispatched task no longer exists".into(),
            data: None,
        });
    }
    // The turn's tier: an explicit `tier` (task API / webhook) wins over the task's declaration;
    // a typo is refused here, before any side effect (a silently-guessed tier would bill the wrong model).
    let tier = route::clean_tier(params.tier.as_deref())?
        .or_else(|| task.as_ref().and_then(|task| task.tier.clone()));

    // Model not configured: spell out "what to do" instead of just dropping a "no key".
    let resolved: Resolved = server.resolved().ok_or_else(|| ErrorObject {
        code: codes::PROVIDER_ERROR,
message: "The model is not configured yet: no API key. One command is enough — /model add <name> <provider> --env VAR (the key goes to the system keychain)."
            .to_string(),
        data: None,
    })?;

    // Pick the model (route.rs): a declared tier always wins; otherwise auto mode decides per
    // message; fixed mode, or "the picked one is already active", return as-is with zero overhead.
    // The fallback and selection reasons go into route_reason, unhidden.
    let (resolved, turn_route) = server
        .resolved_for_turn(resolved, &params.message, tier.as_deref())
        .await;

    // ---- Attachments: staged images, read + base64'd here. Gates on the provider's vision
    // capability and the file's existence **before any side effect** — a bad attachment must
    // not leave a half-turn behind.
    let user_images = resolve_attachments(server, &params, &resolved.provider_id, resolved.vision)?;

    let task_lock = task
        .as_ref()
        .map(|task| server.session_write_lock(&format!("task-control:{}", task.id)));
    let task_guard = match &task_lock {
        Some(lock) => Some(lock.lock().await),
        None => None,
    };
    if let Some(task) = &task {
        let current = server.tasks.get(&task.id).await.map_err(|e| ErrorObject {
            code: codes::INTERNAL_ERROR,
            message: format!("Cannot recheck task admission: {e}"),
            data: None,
        })?;
        let dispatched = joyczl_state::dispatch::TaskDispatch::new(server.pool.clone())
            .get(&task.id)
            .await
            .map_err(|e| ErrorObject {
                code: codes::INTERNAL_ERROR,
                message: format!("Cannot read task dispatch: {e}"),
                data: None,
            })?;
        if !current.as_ref().is_some_and(|t| t.status.is_open())
            || (dispatch.is_none() && dispatched.is_some())
            || (dispatch.is_some()
                && !current
                    .as_ref()
                    .is_some_and(|t| t.status == joyczl_state::TaskStatus::Pending))
        {
            return Err(ErrorObject {
                code: codes::SESSION_BUSY,
                message: "Task admission changed; inspect its current state before continuing"
                    .into(),
                data: None,
            });
        }
    }

    if let Some(task) = &task {
        if let Some(checkpoint) = crate::task::recovery::checkpoint(server, task).await? {
            crate::task::recovery::require_known(&checkpoint)?;
        }
    }
    let turn_id = dispatch
        .as_ref()
        .map(|row| row.turn_id.clone())
        .unwrap_or_else(new_turn_id);

    // ---- One driver per session (0.16), claimed **before any side effect**: a turn refused
    // here must not flip a task to `running` or leave a checkpoint behind. The holder's id goes
    // into the error because it is the only thing the caller can act on.
    let (interrupt, steering) =
        server
            .begin_turn(&turn_id, &session_id)
            .map_err(|holder| ErrorObject {
                code: codes::SESSION_BUSY,
                message: format!(
"A turn is already running in this session ({holder}) — wait for it to finish, or add a line with turn/steer; it is picked up on the turn's next round."
                ),
                data: None,
            })?;
    let _guard = TurnGuard {
        server,
        turn_id: turn_id.clone(),
        session_id: session_id.clone(),
    };

    let acceptance = match &task {
        Some(task) => crate::task::contract::admit(server, sink, task).await?,
        None => Vec::new(),
    };
    if let Some(task) = &task {
        match server.tasks.set_running(&task.id, &turn_id).await {
            Ok(()) => {
                if let Ok(Some(row)) = server.tasks.get(&task.id).await {
                    crate::task::notify(sink, &row, None);
                }
            }
            Err(e) => {
                return Err(ErrorObject {
                    code: codes::INTERNAL_ERROR,
                    message: format!(
                        "Task heartbeat could not be recorded; execution stopped: {e}"
                    ),
                    data: None,
                })
            }
        }
    }
    drop(task_guard);
    let task_id = task.as_ref().map(|task| task.id.clone());

    // The task's objective half: what must be measurably true, and where its paths live.
    // Parsed once per turn — verification runs after every round, and re-parsing the same JSON
    // each time would cost more than the `stat`s it guards.
    let task_root = resolve_turn_cwd(&task.as_ref().and_then(|row| row.cwd.clone()))?;

    let plan = load_plan(server, &resolved, task.as_ref(), &acceptance).await;

    // ---- The run: one execution of the task, which **is** this turn (a run and the turn that
    // carries it share an id, so a checkpoint inside it always knows which attempt it belongs
    // to). No task, no run — an ordinary chat turn has nothing to account for.
    let run_id = open_run(server, task.as_ref(), &turn_id, &session_id).await;
    if task.is_some() && run_id.is_none() {
        return Err(ErrorObject {
            code: codes::INTERNAL_ERROR,
            message: "Task run could not be recorded; execution stopped".into(),
            data: None,
        });
    }
    // Only meaningful while this turn is the one driving the task: the card's "which round is running".
    let round_no = 1i32;
    let max_rounds = task
        .as_ref()
        .map(|row| row.max_rounds.max(1))
        .unwrap_or_else(|| {
            server
                .settings
                .read()
                .expect("the settings lock cannot be poisoned")
                .goal_max_rounds
                .max(1)
        });
    // ---- The checkpoint outlet (shared with the resumed turn — see checkpoint_outlet).
    let checkpoint = checkpoint_outlet(
        &server.checkpoints,
        sink,
        &turn_id,
        &session_id,
        &params.message,
        run_id.as_deref(),
        round_no,
    );

    // Working directory for this turn: validate when provided (fail-closed). Reject before the
    // turn starts, leaving no half-turn behind.
    //
    // Precedence: what **this turn** names → the task's own folder → what the **session**
    // remembers. A task's directory wins over the session's, because "this task owns this
    // folder" would be a lie if the tools writing inside it were bounded by something else.
    let mut turn_cwd = resolve_turn_cwd(&params.cwd)?.or_else(|| task_root.clone());
    if turn_cwd.is_none() {
        turn_cwd = session_cwd(server, &session_id).await;
    }
    // The boundary acceptance checks resolve paths against — same rule, one directory.

    // Only actually-used directories enter "recently used" (workspaces.json) — quick entries for the cockpit picker.
    if let Some(path) = &turn_cwd {
        joyczl_config::workspaces::record_workspace(
            &server.settings().home,
            &path.display().to_string(),
        );
    }

    sink.notification(ServerNotification::TurnStarted(TurnStartedNotification {
        turn_id: turn_id.clone(),
        session_id: session_id.clone(),
        user_message: params.message.clone(),
        ts: Local::now().to_rfc3339(),
    }));

    // ---- SessionStart: the first turn of this session. The returned `additionalContext` merges into this
    // turn's message (the hook may want to lay some background for the model, e.g. "this repo's rules live in CONTRIBUTING.md").
    let mut message = params.message.clone();
    if server
        .chat
        .session_history(&session_id)
        .await
        .unwrap_or_default()
        .is_empty()
    {
        let payload = serde_json::json!({
            "session_id": session_id,
            "cwd": std::env::current_dir().ok(),
            "user_message": params.message,
        });
        let outcome = crate::fire_hook(
            &server.hooks,
            joyczl_tools::hooks::HookEvent::SessionStart,
            payload,
        )
        .await;
        if let Some(context) = outcome.context {
            // Say explicitly that this came from a hook: the model must not assume the user said it.
            message = format!("{context}\n\n{message}");
        }
        if let Some(reason) = outcome.blocked {
            return Err(joyczl_protocol::ErrorObject {
                code: joyczl_protocol::codes::INTERNAL_ERROR,
                message: format!("The SessionStart hook refused this session: {reason}"),
                data: None,
            });
        }
    }
    // The plan travels with the message: context for this round, not a protocol object. Appended
    // last so it sits closest to the work it describes.
    if let Some(current) = &plan {
        message = format!("{message}\n\n{}", current.brief());
    }

    // ---- Retry notifications: **sent on every occurrence**.
    //
    // The counter flows through here too — `meta.retries` is therefore a measured value, not a
    // guess. What the user should see is "rate limited, retrying", not an inexplicably long pause.
    let retries = Arc::new(AtomicI32::new(0));
    let retry_sink = retry_outlet(sink, &turn_id, retries.clone());

    // ---- Front door: the triage graph (off by default).
    //
    // The graph can only make this turn **faster**, never worse: any failure at any step falls back to
    // the ordinary full loop below — the same "fail-open" rule as the retrieval gate.
    //
    // The whole section (graph and full_turn) is wrapped in the notification outlet: model calls may
    // happen anywhere along either path, so retry notifications must not cover only one of them.
    // Annotate the error type explicitly: no more `?` bubbling up here; the compiler needs to know what it is.
    let dispatched: Result<_, joyczl_protocol::ErrorObject> =
        joyczl_provider::retry::with_note_sink(retry_sink, async {
            match graph_route(
                server,
                &resolved,
                &message,
                TurnFrame {
                    session_id: &session_id,
                    turn_id: &turn_id,
                    sink,
                    interrupt: Some(interrupt.clone()),
                    turn_cwd: turn_cwd.clone(),
                    route_reason: turn_route.clone(),
                    steering: Some(steering.clone()),
                    resume: None,
                    checkpoint: Some(checkpoint.clone()),
                },
            )
            .await?
            {
                Some(routed) => Ok((Some(routed.info), routed.turn)),
                None => Ok((
                    None,
                    full_turn(
                        server,
                        &resolved,
                        &message,
                        user_images.clone(),
                        TurnFrame {
                            session_id: &session_id,
                            turn_id: &turn_id,
                            sink,
                            interrupt: Some(interrupt.clone()),
                            turn_cwd: turn_cwd.clone(),
                            route_reason: turn_route.clone(),
                            steering: Some(steering.clone()),
                            resume: None,
                            checkpoint: Some(checkpoint.clone()),
                        },
                        None,
                    )
                    .await?,
                )),
            }
        })
        .await;

    let (graph, full) = match dispatched {
        Ok(pair) => pair,
        Err(e) => {
            // This turn **did not finish** (model died, graph blew up...): close every ledger
            // honestly before leaving — an open checkpoint would be resumed on next boot as if
            // the failure never happened, and the task card would keep saying "running".
            fail_turn(
                server,
                sink,
                &turn_id,
                run_id.as_deref(),
                task_id.as_deref(),
                &e.message,
            )
            .await;
            // StopFailure: this turn **did not finish** (model died, graph blew up...). Observe-only event —
            // problems in the hook itself must not overwrite the original error.
            crate::fire_hook(
                &server.hooks,
                joyczl_tools::hooks::HookEvent::StopFailure,
                serde_json::json!({
                    "session_id": session_id,
                    "turn_id": turn_id,
                    "error": e.message,
                }),
            )
            .await;
            return Err(e);
        }
    };

    let (full, continues, unfinished) = continue_turn(
        server,
        &resolved,
        full,
        TurnFrame {
            session_id: &session_id,
            turn_id: &turn_id,
            sink,
            interrupt: Some(interrupt.clone()),
            turn_cwd: turn_cwd.clone(),
            route_reason: turn_route.clone(),
            steering: Some(steering.clone()),
            resume: None,
            checkpoint: Some(checkpoint.clone()),
        },
        run_id.as_deref(),
        task_id.as_deref(),
        task.as_ref()
            .and_then(|row| row.goal_condition.as_ref())
            .is_some(),
    )
    .await?;

    let driver::DrivenTask {
        full,
        decision,
        goal_status,
        round_no,
    } = driver::drive_task(
        driver::DriveTask {
            server,
            resolved: &resolved,
            task: task.clone(),
            run_id: run_id.clone(),
            frame: TurnFrame {
                session_id: &session_id,
                turn_id: &turn_id,
                sink,
                interrupt: Some(interrupt.clone()),
                turn_cwd: turn_cwd.clone(),
                route_reason: turn_route.clone(),
                resume: None,
                checkpoint: Some(checkpoint.clone()),
                steering: Some(steering.clone()),
            },
            plan,
            round_no,
            max_rounds,
            started,
            wall_offset_ms: 0,
        },
        full,
    )
    .await?;

    let FullTurn {
        result,
        gate,
        history,
    } = full;

    finalize_turn(
        server,
        sink,
        FinishedTurn {
            session_id,
            turn_id: turn_id.clone(),
            user_message: &params.message,
            resolved: &resolved,
            graph,
            gate,
            result,
            route_reason: turn_route.clone(),
            retries: retries.load(Ordering::Relaxed),
            goal_status,
            goal_rounds: round_no,
            history,
            started,
            run_id,
            decision: Some(decision),
            continues,
            unfinished,
        },
    )
    .await?;

    // Only after every notification is out does the response get its turn.
    sink.response(JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id,
        result: serde_json::to_value(TurnStartResponse { turn_id }).expect("serialises"),
    }));
    Ok(())
}
