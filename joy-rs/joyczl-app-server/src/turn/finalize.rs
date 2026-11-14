//! Finalize stage of the turn lifecycle.

use super::{fold_tool_activity, settle_task, HistoryStats, Settlement, TaskDecision};
use crate::{narrow, trace, EventSink, Server};
use chrono::Local;
use joyczl_loop::LoopResult;
use joyczl_protocol::{
    codes, ConsolidationCompletedNotification, ErrorObject, GateDecision, GraphInfo,
    GraphRouteKind, ServerNotification, TokenUsage, ToolCallRecord, ToolCompletedNotification,
    ToolStatus, TurnCompletedNotification, TurnMeta,
};
use joyczl_provider::Resolved;

/// Everything one finished turn leaves behind — shared by the fresh path and the resumed one,
/// so "a resumed turn gets exactly the same accounting" holds by construction, not by copy-paste.
pub(crate) struct FinishedTurn<'a> {
    /// What history/compaction looked like — the trace writes the before/after in turns.
    pub(crate) history: HistoryStats,
    pub(crate) session_id: String,
    pub(crate) turn_id: String,
    pub(crate) user_message: &'a str,
    pub(crate) resolved: &'a Resolved,
    pub(crate) graph: Option<GraphInfo>,
    pub(crate) gate: Option<GateDecision>,
    pub(crate) result: LoopResult,
    pub(crate) route_reason: Option<String>,
    pub(crate) retries: i32,
    pub(crate) goal_status: Option<String>,
    pub(crate) goal_rounds: i32,
    pub(crate) started: std::time::Instant,
    /// The run (task attempt) this turn is; `None` for a task-less turn.
    pub(crate) run_id: Option<String>,
    /// The verdict of this turn's rounds; `None` = nobody decided (a resumed turn).
    pub(crate) decision: Option<TaskDecision>,
    /// How many times this turn continued itself (Stop hook or unfinished work).
    pub(crate) continues: usize,
    /// Why it could not continue further, when it stopped with work still open. `None` = it
    /// stopped because it was done (or was not allowed to try).
    pub(crate) unfinished: Option<String>,
}

/// The shared tail of every turn: notifications, the checkpoint ledger's closing entry, the
/// trace, the usage ledger, the write section (history append → consolidation → MEMORY.md) and
/// the `TurnCompleted` announcement. The reply goes to the response by `run_turn` itself — only
/// it knows the order, and a resumed turn has no response to send at all.
pub(crate) async fn finalize_turn(
    server: &Server,
    sink: &EventSink,
    mut t: FinishedTurn<'_>,
) -> Result<(), ErrorObject> {
    let settings = server.settings();
    let Settlement {
        carried_task,
        waiting_for_jobs,
        input_wait_committed,
    } = settle_task(server, sink, &mut t).await?;
    let FinishedTurn {
        session_id,
        turn_id,
        user_message,
        resolved,
        graph,
        gate,
        result,
        route_reason,
        retries,
        goal_status,
        goal_rounds,
        history,
        started,
        run_id: _,
        decision: _,
        continues,
        unfinished,
    } = t;

    crate::completion_delivery::flush(server).await;

    for call in &result.tool_calls {
        sink.notification(ServerNotification::ToolCompleted(
            ToolCompletedNotification {
                turn_id: turn_id.clone(),
                tool_use_id: call.tool_use_id.clone(),
                tool: call.name.clone(),
                output: call.output.clone(),
                status: if call.ok() {
                    ToolStatus::Ok
                } else {
                    ToolStatus::Error
                },
                duration_ms: Some(narrow(call.duration_ms)),
            },
        ));
    }

    // Tool activity is folded into the assistant text. Without this, the model forgets what it already
    // did and does it again next turn (that is how triple-booked meetings happen).
    let mut reply = fold_tool_activity(&result.reply, &result.tool_calls);
    if result.stop_reason == joyczl_provider::StopReason::MaxTokens && !result.interrupted {
        reply.push_str("\n\n(The output limit was reached; this response is incomplete.)");
    }
    if waiting_for_jobs {
        reply.push_str("\n\n(Background jobs are still running; task completion is pending.)");
    }

    if input_wait_committed {
        reply.push_str("\n\n(Waiting for the declared input; task completion is pending.)");
    }

    // The quick answer came from the small model. Record that honestly — so "what model are you" won't be answered wrong by the model itself.
    let quick = graph
        .as_ref()
        .is_some_and(|info| info.route == GraphRouteKind::Quick);
    let meta = TurnMeta {
        gate,
        graph,
        iterations: result.iterations,
        latency_ms: started.elapsed().as_millis() as i32,
        tools: result
            .tool_calls
            .iter()
            .map(|c| ToolCallRecord {
                tool: c.name.clone(),
                status: if c.ok() {
                    ToolStatus::Ok
                } else {
                    ToolStatus::Error
                },
                duration_ms: Some(narrow(c.duration_ms)),
            })
            .collect(),
        model: if quick {
            resolved.small_model.clone()
        } else {
            resolved.model.clone()
        },
        provider: resolved.provider_id.clone(),
        usage: Some(TokenUsage {
            input_tokens: narrow(result.usage.input_tokens),
            output_tokens: narrow(result.usage.output_tokens),
        }),
        interrupted: result.interrupted,
        guard_hits: result.guard.hits,
        guard_note: result.guard.note.clone(),
        retries,
        goal_status: goal_status.clone(),
        goal_rounds,
        route_reason: route_reason.clone(),
    };

    let meta_json = serde_json::to_string(&meta).ok();

    // ---- Persist: one trace line + one usage line. Observability is not optional, but it must not drag
    // the conversation down either — write failures inside the two functions only shout to stderr.
    trace::record_turn(
        &settings.home,
        &serde_json::json!({
            "ts": Local::now().to_rfc3339(),
            "turnId": turn_id,
            "sessionId": session_id,
            "userMessage": user_message,
            "reply": reply,
            "meta": meta,
            // ---- Flattened, so `jq` on a day's trace does not need to remember `meta`'s shape.
            // `callIndex` is the **request** order: with read-only tools running in parallel that
            // is the only order the caller cares about (the wall-clock order is in `durationMs`).
            "tools": result
                .tool_calls
                .iter()
                .enumerate()
                .map(|(index, call)| serde_json::json!({
                    "tool": call.name,
                    "callIndex": index,
                    "durationMs": call.duration_ms,
                    "status": if call.ok() { "ok" } else { "error" },
                }))
                .collect::<Vec<_>>(),
            "retries": retries,
            "tokens": {
                "input": result.usage.input_tokens,
                "output": result.usage.output_tokens,
                // Two numbers for the same request: dividing them is the estimate's error, which
                // is what calibrates the next turn's budget.
                "estimatedInput": result.estimated_input_tokens,
                "observedInput": result.observed_input_tokens,
                // The turn-level cost cap, when one is set (JOY_TURN_TOKEN_BUDGET; 0 = off).
                "turnBudget": settings.turn_token_budget,
            },
            // Compaction is "before/after" in **turns**: how many the session had, how many survived.
            "history": {
                "turnsTotal": history.turns_total,
                "turnsKept": history.turns_kept,
                "compacted": history.compacted,
            },
            // Why it stopped: steps spent, how many times it continued itself, and — when it gave up
            // with work still open — what was left. Without these three, "it stopped short" is an
            // opinion; with them it is a number.
            "steps": result.iterations,
            "continues": continues,
            "unfinished": unfinished,
        }),
    );
    trace::record_usage(
        &settings.home,
        &serde_json::json!({
            "ts": Local::now().to_rfc3339(),
            "turnId": turn_id,
            "sessionId": session_id,
            "provider": meta.provider,
            "model": meta.model,
            "inputTokens": result.usage.input_tokens,
            "outputTokens": result.usage.output_tokens,
            "iterations": result.iterations,
            "interrupted": result.interrupted,
        }),
    );

    // Usage ledger (migration 0010): bookkeeping only, never enforced. A write failure does not block
    // the turn — bookkeeping is a side path, not the main road; a note on stderr is enough.
    if let Err(e) = server
        .usage
        .record(
            &session_id,
            &meta.provider,
            &meta.model,
            result.usage.input_tokens,
            result.usage.output_tokens,
        )
        .await
    {
        eprintln!("(joy) usage not recorded in the ledger: {e}");
    }

    // ---- Write section: per-session serialisation. From the history append to the MEMORY.md
    // mirror, this session's writes must not interleave with another concurrent turn's write
    // section — that is the lock's incremental value: the three SQLite guards stop double-write
    // corruption, this stops read/write interleaving. Model calls never hold it.
    let session_write_lock = server.session_write_lock(&session_id);
    let write_guard = session_write_lock.lock().await;

    // Everything steered into this turn, in arrival order — including anything accepted after
    // the loop's last boundary: it is written down even if no model call could see it, so the
    // next turn's history carries it instead of it vanishing.
    let steers = server
        .steering_of(&turn_id)
        .map(|steering| {
            steering
                .lock()
                .expect("the steering lock cannot be poisoned")
                .all()
        })
        .unwrap_or_default();
    server
        .chat
        .append_turn_for(
            user_message,
            &steers,
            &reply,
            &session_id,
            "app-server",
            meta_json.as_deref(),
            carried_task.as_ref().map(|task| task.id.as_str()),
        )
        .await
        .map_err(|e| ErrorObject {
            code: codes::INTERNAL_ERROR,
            message: format!("Could not write the conversation log: {e}"),
            data: None,
        })?;

    // ---- The ledger: the record that `chat_log` above is a *view* of. Same moment, same
    // lock. A failure here is a reporting loss — the reply is already persisted — so it
    // shouts on stderr and the turn carries on, exactly like a tool error.
    let ledger_sink = crate::ledger_writer::LedgerSink::new(
        &settings.home,
        settings.ledger_inline_max.max(0) as usize,
    );
    let ledger_entries = crate::ledger_writer::entries_for_turn(
        &ledger_sink,
        &session_id,
        &turn_id,
        user_message,
        &result.messages,
        &reply,
    );
    if let Err(e) = server
        .ledger
        .append(&session_id, &turn_id, &ledger_entries, user_message)
        .await
    {
        eprintln!(
            "(joy) ledger write failed (the conversation is stored; this turn is unaffected): {e}"
        );
    }

    // ---- Calibrate estimates with measurements: save this turn's last request's (measured prefill, local estimate);
    // next turn's budget math corrects by the ratio. Failure only shouts — calibration is an optimization, not this turn's ledger.
    if let Err(e) = server
        .chat
        .observe_context(
            &session_id,
            result.observed_input_tokens as i64,
            result.estimated_input_tokens as i64,
        )
        .await
    {
        eprintln!("(joy) observed context size not recorded (this turn is unaffected): {e}");
    }

    if waiting_for_jobs {
        if let Some(task) = &carried_task {
            if let Ok(Some(wait)) = joyczl_state::waits::JobWaits::new(server.pool.clone())
                .get(&task.id)
                .await
            {
                if wait.state == "waiting" {
                    crate::task::waits::arm(server.clone(), sink.clone(), wait);
                }
            }
        }
    }

    if input_wait_committed {
        if let Some(task) = &carried_task {
            if let Ok(Some(wait)) =
                joyczl_state::external_waits::ExternalWaits::new(server.pool.clone())
                    .waiting(&task.id)
                    .await
            {
                if wait.state == "waiting" {
                    crate::input_wake::arm(server.clone(), sink.clone(), wait);
                }
            }
        }
    }

    // ---- Memory work is the worker's job now: the turn ends when the reply is on disk.
    //
    // The worker decides whether distillation is due (its trigger is unchanged: enough new
    // rows) and mirrors the plaintext library afterwards. Nudging it is all that happens on
    // this clock — no model call, no file rewrite, no held lock. `boot()` without a worker
    // (tests, embedders) keeps the old synchronous path: one function, two callers.
    if let Some(worker) = server.memory_worker.as_ref() {
        worker.wake();
    } else {
        let new_facts = joyczl_memory::consolidation::consolidate_if_due(
            &server.chat,
            &server.facts,
            &server.episodes,
            resolved.client.as_ref(),
            &resolved.small_model,
            settings.consolidate_every,
        )
        .await
        .unwrap_or(0);
        if new_facts > 0 {
            sink.notification(ServerNotification::ConsolidationCompleted(
                ConsolidationCompletedNotification {
                    new_facts: new_facts as i32,
                },
            ));
        }
        if let Err(e) =
            joyczl_memory::export_markdown(&server.facts, &server.episodes, &settings.home).await
        {
            eprintln!("(joy) MEMORY.md mirror failed (memory itself is unaffected): {e}");
        }
    }
    // Write section ends here: notifications and the response are not writes — don't let the
    // lock linger a step longer.
    drop(write_guard);

    sink.notification(ServerNotification::TurnCompleted(Box::new(
        TurnCompletedNotification {
            turn_id,
            reply: result.reply.clone(),
            iterations: result.iterations,
            usage: Some(TokenUsage {
                input_tokens: narrow(result.usage.input_tokens),
                output_tokens: narrow(result.usage.output_tokens),
            }),
            meta,
        },
    )));
    Ok(())
}
