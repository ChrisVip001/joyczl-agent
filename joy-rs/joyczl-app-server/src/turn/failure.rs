//! Error finalization preserves human controls and commits exact terminal identities together.
use crate::{EventSink, Server};
use joyczl_state::{RunStatus, TaskStatus};

async fn checkpoint(server: &Server, turn_id: &str, status: &str) {
    if let Err(error) = server.checkpoints.finish(turn_id, status).await {
        eprintln!("(joy) failed turn checkpoint could not be closed: {error}");
    }
}
async fn close_run(server: &Server, id: Option<&str>, status: RunStatus, reason: &str) {
    if let Some(id) = id {
        if let Err(error) = server.runs.close(id, status, Some(reason)).await {
            eprintln!("(joy) failed run could not be closed: {error}");
        }
    }
}

pub(crate) async fn finish(
    server: &Server,
    sink: &EventSink,
    turn_id: &str,
    run_id: Option<&str>,
    task_id: Option<&str>,
    message: &str,
) {
    let Some(task_id) = task_id else {
        checkpoint(server, turn_id, "abandoned").await;
        close_run(server, run_id, RunStatus::Failed, message).await;
        return;
    };
    let control = server.session_write_lock(&format!("task-control:{task_id}"));
    let _control = control.lock().await;
    let task = match server.tasks.get(task_id).await {
        Ok(Some(task)) => task,
        outcome => {
            eprintln!(
                "(joy) task state unavailable during failure; retained for review: {outcome:?}"
            );
            checkpoint(server, turn_id, "suspended").await;
            close_run(server, run_id, RunStatus::Waiting, message).await;
            return;
        }
    };
    if task
        .current_turn_id
        .as_deref()
        .is_some_and(|current| current != turn_id)
    {
        eprintln!("(joy) stale turn failure retained without changing the newer task execution");
        checkpoint(server, turn_id, "abandoned").await;
        close_run(server, run_id, RunStatus::Failed, message).await;
        return;
    }
    // A provider error can arrive after successful calls already reached a durable boundary.
    let charged = async {
        if let Some(row) = server
            .checkpoints
            .get(turn_id)
            .await
            .map_err(|e| e.to_string())?
        {
            if row.run_id.as_deref() != run_id {
                return Err("Failed checkpoint belongs to a different execution".to_string());
            }
            let usage = crate::task::contract::checkpoint_usage(&row.usage_json)?;
            crate::task::contract::charge_checkpoint(server, &row, usage).await?;
        }
        Ok::<(), String>(())
    }
    .await;
    let accounting_failed = charged.is_err();
    let reason = match charged {
        Ok(()) => message.to_string(),
        Err(error) => {
            let reason = format!("{message}; known checkpoint usage could not be charged: {error}");
            eprintln!("(joy) {reason}");
            reason
        }
    };
    let message = reason.as_str();
    let preserved = match task.status {
        TaskStatus::Cancelled => Some((RunStatus::Cancelled, "abandoned")),
        TaskStatus::Done => Some((RunStatus::Completed, "done")),
        TaskStatus::Failed => Some((RunStatus::Failed, "abandoned")),
        TaskStatus::Suspended => Some((RunStatus::Suspended, "suspended")),
        TaskStatus::NeedsAttention => Some((RunStatus::Waiting, "suspended")),
        _ => None,
    };
    if let Some((status, boundary)) = preserved {
        checkpoint(server, turn_id, boundary).await;
        close_run(
            server,
            run_id,
            status,
            task.last_stop_reason
                .as_deref()
                .unwrap_or("Preserved existing task stop"),
        )
        .await;
        crate::task::notify(sink, &task, task.last_stop_reason.as_deref());
        return;
    }
    if accounting_failed {
        if let Err(error) = server.checkpoints.review(turn_id, run_id, message).await {
            eprintln!("(joy) failed accounting review could not be committed: {error}");
        }
    } else {
        checkpoint(server, turn_id, "abandoned").await;
        if let Err(error) = crate::completion_delivery::finish_task(
            server,
            task_id,
            TaskStatus::Failed,
            Some(message),
            turn_id,
            run_id,
        )
        .await
        {
            eprintln!(
                "(joy) turn failure could not be committed: {}",
                error.message
            );
        }
    }
    if let Ok(Some(task)) = server.tasks.get(task_id).await {
        crate::task::notify(sink, &task, Some(message));
    }
}
