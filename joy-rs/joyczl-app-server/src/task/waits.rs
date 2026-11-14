//! Wait only for jobs belonging to a task already authorized by a human.
use crate::{EventSink, Server};
use joyczl_state::waits::{JobWait, JobWaits, JOB_WAIT_REASON};

pub(crate) fn arm(server: Server, sink: EventSink, wait: JobWait) {
    tokio::spawn(async move {
        let waits = JobWaits::new(server.pool.clone());
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let current = match waits.get(&wait.task_id).await {
                Ok(Some(row))
                    if row.state == "waiting"
                        && row.run_id == wait.run_id
                        && row.turn_id == wait.turn_id =>
                {
                    row
                }
                Ok(_) => return,
                Err(e) => {
                    eprintln!("(joy) task wait unavailable: {e}");
                    continue;
                }
            };
            let task = match server.tasks.get(&wait.task_id).await {
                Ok(Some(task)) => task,
                Ok(None) => return,
                Err(e) => {
                    eprintln!("(joy) waiting task unavailable: {e}");
                    continue;
                }
            };
            if task.status != joyczl_state::TaskStatus::Suspended
                || task.suspend_reason.as_deref() != Some(JOB_WAIT_REASON)
            {
                let _ = waits.cancel(&wait.task_id).await;
                return;
            }
            let jobs = match waits.jobs(&wait.task_id).await {
                Ok(jobs) => jobs,
                Err(e) => {
                    eprintln!("(joy) waiting job states unavailable: {e}");
                    continue;
                }
            };
            if jobs.iter().any(|job| job.status == "running") {
                continue;
            }
            if jobs.is_empty()
                || jobs
                    .iter()
                    .any(|job| matches!(job.status.as_str(), "interrupted" | "killed"))
            {
                let reason =
                    "A background job was interrupted or stopped; reconcile before continuing";
                let lock = server.session_write_lock(&format!("task-control:{}", wait.task_id));
                let _guard = lock.lock().await;
                if let Ok(Some(task)) = server.tasks.get(&wait.task_id).await {
                    if task.status == joyczl_state::TaskStatus::Suspended
                        && task.suspend_reason.as_deref() == Some(JOB_WAIT_REASON)
                    {
                        let _ = server.tasks.needs_attention(&wait.task_id, reason).await;
                        let _ = server.checkpoints.finish(&wait.turn_id, "suspended").await;
                        let _ = waits.cancel(&wait.task_id).await;
                        eprintln!("(joy) task {}: {reason}", wait.task_id);
                        if let Ok(Some(task)) = server.tasks.get(&wait.task_id).await {
                            crate::task::notify(&sink, &task, Some(reason));
                        }
                    }
                }
                return;
            }
            if server.resolved().is_none()
                || !server.active_turns(Some(&wait.session_id)).is_empty()
            {
                continue;
            }
            if crate::resume::resume_waiting(&server, &sink, current, &jobs).await {
                return;
            }
        }
    });
}
