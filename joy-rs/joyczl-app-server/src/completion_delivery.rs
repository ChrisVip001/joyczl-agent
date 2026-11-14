//! Dispatch only completion receipts committed with their task status.
use crate::Server;
use joyczl_state::completions::Completions;
use joyczl_tools::notify::{Notifier, NotifyEvent};
use std::sync::Arc;

pub(crate) fn start(
    rows: &Arc<Completions>,
    notifier: Option<&Arc<dyn Notifier>>,
    lock: &Arc<tokio::sync::Mutex<()>>,
    home: std::path::PathBuf,
) {
    let (rows, notifier, lock) = (
        Arc::downgrade(rows),
        notifier.map(Arc::downgrade),
        Arc::downgrade(lock),
    );
    tokio::spawn(async move {
        loop {
            {
                let (Some(rows), Some(lock)) = (rows.upgrade(), lock.upgrade()) else {
                    return;
                };
                let _guard = lock.lock().await;
                crate::schedule_output::deliver(&rows.schedules(), &home).await;
                if let Some(notifier) = notifier.as_ref().and_then(|notifier| notifier.upgrade()) {
                    deliver(&rows, notifier.as_ref()).await;
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}

pub(crate) async fn flush(server: &Server) {
    let _guard = server.completion_lock.lock().await;
    crate::schedule_output::deliver(&server.completions.schedules(), &server.settings().home).await;
    if let Some(notifier) = &server.notifier {
        deliver(&server.completions, notifier.as_ref()).await;
    }
}

pub(crate) async fn deliver(rows: &Completions, notifier: &dyn Notifier) {
    let pending = match rows.pending().await {
        Ok(rows) => rows,
        Err(error) => {
            eprintln!("(joy) task completion outbox unavailable: {error}");
            return;
        }
    };
    let target = notifier.delivery_target();
    for row in pending {
        if row.notify_target != target {
            eprintln!(
                "(joy) task {} completion retained: notification destination changed",
                row.task_id
            );
            continue;
        }
        let event = if row.is_goal {
            NotifyEvent::GoalFinished {
                session_id: row.session_id.unwrap_or_default(),
                status: match row.status.as_str() {
                    "done" => "satisfied",
                    "needs_attention" => "blocked",
                    other => other,
                }
                .into(),
                reason: row.reason,
            }
        } else {
            NotifyEvent::TaskFinished {
                id: row.task_id.clone(),
                title: row.title,
                status: row.status,
                reason: row.reason,
            }
        };
        match notifier.enqueue(event) {
            Ok(()) => {
                if let Err(error) = rows.enqueued(row.id).await {
                    eprintln!(
                        "(joy) completion {} enqueue receipt could not be recorded: {error}",
                        row.id
                    );
                }
            }
            Err(error) => eprintln!(
                "(joy) task {} completion retained for retry: {error}",
                row.task_id
            ),
        }
    }
    let schedules = rows.schedules();
    match schedules.pending().await {
        Err(error) => eprintln!("(joy) schedule completion outbox unavailable: {error}"),
        Ok(pending) => {
            for row in pending {
                if row.notify_target != target {
                    eprintln!(
                        "(joy) schedule '{}' completion retained: notification destination changed",
                        row.name
                    );
                    continue;
                }
                let preview = if row.preview.is_empty() {
                    row.detail.chars().take(120).collect()
                } else {
                    row.preview
                };
                let event = NotifyEvent::ScheduleRan {
                    name: row.name.clone(),
                    status: row.status,
                    preview,
                };
                match notifier.enqueue(event) {
                    Ok(()) => {
                        if let Err(error) = schedules.enqueued(&row.name, &row.minute).await {
                            eprintln!(
                                "(joy) schedule completion enqueue receipt not recorded: {error}"
                            );
                        }
                    }
                    Err(error) => eprintln!(
                        "(joy) schedule '{}' completion retained for retry: {error}",
                        row.name
                    ),
                }
            }
        }
    }
}

/// A completion cannot be reported if its status/production receipt did not commit.
pub(crate) async fn finish_task(
    server: &Server,
    task_id: &str,
    status: joyczl_state::TaskStatus,
    reason: Option<&str>,
    turn_id: &str,
    run_id: Option<&str>,
) -> Result<(), joyczl_protocol::ErrorObject> {
    if let Err(error) = server
        .tasks
        .finish_run(task_id, status, reason, run_id)
        .await
    {
        let message =
            format!("Task outcome could not be committed; completion is not confirmed: {error}");
        eprintln!("(joy) {message}");
        if let Err(error) = server.checkpoints.finish(turn_id, "suspended").await {
            eprintln!("(joy) checkpoint could not be retained for review: {error}");
        }
        if let Some(run_id) = run_id {
            if let Err(error) = server
                .runs
                .close(run_id, joyczl_state::RunStatus::Waiting, Some(&message))
                .await
            {
                eprintln!("(joy) run could not be retained for review: {error}");
            }
        }
        if let Err(error) = server.tasks.needs_attention(task_id, &message).await {
            eprintln!("(joy) task attention state could not be recorded: {error}");
        }
        return Err(joyczl_protocol::ErrorObject {
            code: joyczl_protocol::codes::INTERNAL_ERROR,
            message,
            data: None,
        });
    }
    Ok(())
}
