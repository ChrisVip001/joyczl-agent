//! Continuation stage of the turn lifecycle.

use super::{fail_turn, full_turn, unfinished_work, FullTurn, TurnFrame};
use crate::Server;
use joyczl_protocol::ErrorObject;
use joyczl_provider::Resolved;

pub(super) async fn continue_turn(
    server: &Server,
    resolved: &Resolved,
    mut full: FullTurn,
    frame: TurnFrame<'_>,
    run_id: Option<&str>,
    task_id: Option<&str>,
    has_goal: bool,
) -> Result<(FullTurn, usize, Option<String>), ErrorObject> {
    let settings = server.settings();
    let auto_continue_max = settings.auto_continue_max.max(0) as usize;

    let mut continues = 0usize;
    // Steps from earlier passes. `full` is **replaced** by the continuation, and the turn's tool
    // results (and their `toolCompleted` notifications, emitted at finalize from `result`) would
    // otherwise cover only the last pass — the client would show a second pass of calls whose
    // earlier results never arrived.
    let mut earlier_calls: Vec<joyczl_loop::ToolOutcome> = Vec::new();
    // The ceiling this pass ran under. A continuation raises it by one slice, and "were we cut
    // off?" is `iterations >= cap` — not a guess about how the model felt about stopping.
    let mut cap = settings.max_iterations.max(1);
    // Set when the turn stops with work still open: either the continuation budget is gone, or the
    // model chose to stop. Both belong in the trace — "stopped short" should be a fact, not a mood.
    let unfinished: Option<String> = loop {
        let payload = serde_json::json!({
            "session_id": frame.session_id,
            "turn_id": frame.turn_id,
            "reply": full.result.reply,
            "iterations": full.result.iterations,
        });
        let outcome =
            crate::fire_hook(&server.hooks, joyczl_tools::hooks::HookEvent::Stop, payload).await;
        let blocked = outcome.blocked;
        // The hook is the human speaking, and it can always demand one more pass. Only when it says
        // nothing does the built-in gate look at facts — and it opens **only when the turn was cut
        // off** (`iterations >= cap`), because that is what "ran out of steps with work left" means.
        //
        // Why not on a natural stop too: a turn that writes a plan and answers "here is the plan" is
        // finished work, not unfinished work. Driving that one to keep going would execute a plan
        // nobody asked to execute. A model that stops early with an open todo list is visible in
        // the trace as `unfinished`, and one word from the human continues it.
        //
        // A goal-carrying turn is driven by the goal loop below — one driver per turn.
        let open_todos = if !has_goal {
            unfinished_work(server, frame.session_id, &full.result)
        } else {
            None
        };
        let cut_off = full.result.iterations >= cap;
        let reason = blocked.or_else(|| open_todos.clone().filter(|_| cut_off));
        let Some(reason) = reason else {
            // Stopped of its own accord; if work is still open, say so in the trace rather than
            // hijack the ending.
            break open_todos;
        };
        if continues >= auto_continue_max {
            // Asked for more after we already continued: say so honestly rather than pretend the
            // work is done.
            full.result.reply.push_str(&format!(
                "\n\n(One step short: {reason}. The auto-continue budget is spent — say \"carry on\" and I resume where it stopped.)"
            ));
            break Some(reason);
        }
        continues += 1;
        eprintln!("(joy) auto-continue {continues} of {auto_continue_max}: {reason}");
        let nudge = format!(
            "{reason}\n\nCarry on: continue from where it stopped, do not restate what is done, do not re-plan."
        );
        // Slice, don't restart: one continuation buys `JOY_TURN_STEP_SLICE` more steps, counted
        // on top of the ones already spent.
        cap = full.result.iterations + settings.turn_step_slice.max(1);
        let resume = joyczl_loop::Resume {
            round_id: full.result.round_id.clone(),
            messages: full.result.messages.clone(),
            completed_iterations: full.result.iterations,
            usage: full.result.usage,
            max_iterations: Some(cap),
        };
        let continued = full_turn(
            server,
            resolved,
            &nudge,
            Vec::new(),
            TurnFrame {
                session_id: frame.session_id,
                turn_id: frame.turn_id,
                sink: frame.sink,
                interrupt: frame.interrupt.clone(),
                turn_cwd: frame.turn_cwd.clone(),
                route_reason: frame.route_reason.clone(),
                steering: frame.steering.clone(),
                resume: Some(resume),
                checkpoint: frame.checkpoint.clone(),
            },
            None,
        )
        .await;
        match continued {
            Ok(next) => {
                earlier_calls.append(&mut full.result.tool_calls);
                full = next;
            }
            Err(e) => {
                fail_turn(
                    server,
                    frame.sink,
                    frame.turn_id,
                    run_id,
                    task_id,
                    &e.message,
                )
                .await;
                return Err(e);
            }
        }
    };

    // Re-attach the earlier passes' calls **before** the last pass's, so the turn reports one
    // request order across the whole turn (and every `toolCompleted` finally has its row).
    if !earlier_calls.is_empty() {
        earlier_calls.append(&mut full.result.tool_calls);
        full.result.tool_calls = earlier_calls;
    }

    Ok((full, continues, unfinished))
}
