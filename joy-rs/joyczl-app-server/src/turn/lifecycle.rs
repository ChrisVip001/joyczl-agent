//! Lifecycle stage of the turn lifecycle.

use super::run_turn;
use crate::{narrow, EventSink, Server};
use joyczl_protocol::{
    codes, RequestId, RetryNotification, ServerNotification, TurnCheckpointedNotification,
    TurnStartParams,
};
use joyczl_state::TaskRow;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

/// The checkpoint outlet, constructed once per turn (fresh or resumed): every tool-results
/// boundary lands in state.db before execution advances. A failed save stops the turn;
/// an unknown in-flight tool batch remains suspended rather than being replayed.
pub(crate) fn checkpoint_outlet(
    checkpoints: &joyczl_state::Checkpoints,
    sink: &EventSink,
    turn_id: &str,
    session_id: &str,
    user_message: &str,
    run_id: Option<&str>,
    round_no: i32,
) -> joyczl_loop::CheckpointSink {
    let (checkpoints, session, user_message) = (
        checkpoints.clone(),
        session_id.to_string(),
        user_message.to_string(),
    );
    let (sink, turn_id) = (sink.clone(), turn_id.to_string());
    // The run is fixed for the whole turn, so the outlet can capture it by value — every
    // boundary of this attempt records the same run id.
    let run = run_id.map(str::to_string);
    Arc::new(move |cp: joyczl_loop::Checkpoint| {
        let (checkpoints, session, user_message) =
            (checkpoints.clone(), session.clone(), user_message.clone());
        let (sink, turn_id) = (sink.clone(), turn_id.clone());
        let run = run.clone();
        Box::pin(async move {
            let messages_json = serde_json::to_string(&cp.messages).unwrap_or_default();
            let usage_json = serde_json::to_string(&cp.usage).unwrap_or_default();
            let operations: Vec<_> = cp
                .operations
                .into_iter()
                .map(|operation| joyczl_state::operations::OperationSnapshot {
                    tool_use_id: operation.tool_use_id,
                    tool: operation.tool,
                    input_json: operation.input.to_string(),
                    output: operation.output,
                })
                .collect();
            if let Err(e) = checkpoints
                .save_with_operations(
                    joyczl_state::checkpoints::CheckpointWrite {
                        turn_id: &turn_id,
                        session_id: &session,
                        user_message: &user_message,
                        messages_json: &messages_json,
                        iterations: cp.iterations,
                        usage_json: &usage_json,
                        run_id: run.as_deref(),
                        round_id: Some(&cp.round_id),
                        round_no: Some(round_no),
                    },
                    &operations,
                )
                .await
            {
                eprintln!("(joy) checkpoint persistence failed; stopping execution: {e}");
                return Err(format!("checkpoint persistence failed: {e}"));
            }
            sink.notification(ServerNotification::TurnCheckpointed(
                TurnCheckpointedNotification {
                    turn_id,
                    session_id: session,
                    iterations: cp.iterations,
                },
            ));
            Ok(())
        })
    })
}

/// Deregistration guard for a turn: however this turn exits (success, error, interrupted),
/// the cancellation token is removed from the table — a finished turn has nothing left to interrupt.
pub(crate) struct TurnGuard<'a> {
    pub(crate) server: &'a Server,
    pub(crate) turn_id: String,
    pub(crate) session_id: String,
}

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        self.server.finish_turn(&self.turn_id);
        // The session is free again: whatever was queued behind this turn gets a turn of its
        // own now. Spawned because `Drop` cannot await, and safe because `finish_turn` above
        // already released the gate — the drain's turn claims it like any other caller.
        let server = self.server.clone();
        let session_id = self.session_id.clone();
        tokio::spawn(async move { drain_queue(&server, &session_id).await });
    }
}

/// Hand the next queued message to a turn of its own, if there is one.
///
/// Called when a turn ends (`TurnGuard::drop`) and once at boot. One message per call: the turn
/// it starts ends and calls back in, so the queue advances one at a time without any loop here.
pub(crate) async fn drain_queue(server: &Server, session_id: &str) {
    match server.queue.pop(session_id).await {
        Ok(Some(row)) => spawn_queued_turn(server.clone(), row),
        Ok(None) => {}
        Err(e) => eprintln!("(joy) queued instruction could not be claimed: {e}"),
    }
}

/// Run one queued message as its own turn, with **no client attached**.
///
/// `EventSink` already tolerates that ("a send failure has only one possible cause: the writer
/// is gone"), so the notifications simply expire. What must never be silent is a **failure**: a
/// queued turn runs while the human is away, so the error has to land in the transcript.
pub(super) fn spawn_queued_turn(server: Server, row: joyczl_state::QueuedMessage) {
    tokio::spawn(async move {
        let session_id = row.session_id;
        let message = row.message;
        let (sink, receiver) = EventSink::channel();
        drop(receiver); // nobody is listening; frames expire on send

        let params = TurnStartParams {
            session_id: Some(session_id.clone()),
            message: message.clone(),
            ..TurnStartParams::default()
        };
        let Err(error) = run_turn(&server, params, RequestId::Number(0), &sink).await else {
            if let Err(e) = server.queue.complete(row.id).await {
                eprintln!("(joy) queued instruction completion could not be recorded: {e}");
                return;
            }
            // TurnGuard may have drained before this claim was released.
            drain_queue(&server, &session_id).await;
            return;
        };

        // Losing the session to another client is not a failure of *this message*: put it back in
        // line (it drains when that turn ends) instead of inventing an error the person never
        // caused.
        if error.code == codes::SESSION_BUSY {
            if let Err(e) = server.queue.release(row.id).await {
                eprintln!("(joy) queued message could not be put back in the queue: {e}");
            }
            return;
        }

        // A failure may follow partial execution: never claim that no action ran.
        let note = format!(
            "(this queued turn failed; review prior actions before retrying: {})",
            error.message
        );
        if let Err(e) = server
            .chat
            .append_exchange_for(&message, &note, &session_id, "app-server", None, None)
            .await
        {
            eprintln!("(joy) the queued message's failure could not be recorded either: {e}");
            return;
        }
        if let Err(e) = server.queue.complete(row.id).await {
            eprintln!(
                "(joy) queued instruction failure acknowledgement could not be recorded: {e}"
            );
            return;
        }
        drain_queue(&server, &session_id).await;
    });
}

// ---- Task rounds ---------------------------------------------------------------------------
//
// A round is one pass of THE LOOP. Everything a task needs — the run account, the step ledger,
// the artifacts, the spend and the acceptance verdict — is written at each round boundary, in
// this order: **observe → record → measure → decide**. The judge (a model) only ever speaks
// after the measurements, and never over them.

/// Open this turn's run. The run id **is** the turn id: a run and the turn carrying it are the
/// same attempt, so a checkpoint written inside it always points at the right row. A task-less
/// turn has no run at all — an ordinary chat has nothing to account for.
pub(super) async fn open_run(
    server: &Server,
    task: Option<&TaskRow>,
    turn_id: &str,
    session_id: &str,
) -> Option<String> {
    let task = task?;
    let target = server
        .notifier
        .as_ref()
        .and_then(|notifier| notifier.delivery_target());
    if let Err(error) = server
        .completions
        .bind(&task.id, server.notifier.is_some(), target.as_deref())
        .await
    {
        eprintln!(
            "(joy) completion delivery intent could not be recorded; execution stopped: {error}"
        );
        return None;
    }
    match server
        .runs
        .start(turn_id, &task.id, Some(session_id), Some(turn_id))
        .await
    {
        Ok(()) => Some(turn_id.to_string()),
        Err(e) => {
            eprintln!("(joy) run record could not be opened (this turn is unaccounted): {e}");
            None
        }
    }
}

/// A turn that failed inside the loop (model died, graph blew up): close the checkpoint, the
/// run and the task honestly, so nothing is left looking "still running".
pub(crate) async fn fail_turn(
    server: &Server,
    sink: &EventSink,
    turn_id: &str,
    run_id: Option<&str>,
    task_id: Option<&str>,
    message: &str,
) {
    crate::turn::failure::finish(server, sink, turn_id, run_id, task_id, message).await;
}

pub(super) fn new_turn_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("t{nanos}")
}

/// Forward provider retries through the same outlet as the running turn.
pub(super) fn retry_outlet(
    sink: &EventSink,
    turn_id: &str,
    retries: Arc<AtomicI32>,
) -> joyczl_provider::retry::NoteSink {
    {
        let counter = retries;
        let sink = sink.clone();
        let turn_id = turn_id.to_string();
        Arc::new(move |notice: joyczl_provider::retry::RetryNotice| {
            counter.fetch_add(1, Ordering::Relaxed);
            sink.notification(ServerNotification::Retry(RetryNotification {
                turn_id: turn_id.clone(),
                attempt: narrow(notice.attempt as i64),
                reason: notice.reason.clone(),
                delay_ms: narrow(notice.delay_ms as i64),
            }));
        })
    }
}
