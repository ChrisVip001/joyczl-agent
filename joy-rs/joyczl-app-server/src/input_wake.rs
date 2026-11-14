//! External data releases a durable wait, never a new task or additional permissions.
use crate::{EventSink, Server};
use joyczl_state::external_waits::{ExternalWaits, InputWait, INPUT_WAIT_REASON};

pub(crate) fn arm(server: Server, sink: EventSink, wait: InputWait) {
    tokio::spawn(async move {
        let rows = ExternalWaits::new(server.pool.clone());
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let current = match rows.waiting(&wait.task_id).await {
                Ok(Some(current))
                    if current.state == "waiting"
                        && current.condition_id == wait.condition_id
                        && current.run_id == wait.run_id
                        && current.turn_id == wait.turn_id =>
                {
                    current
                }
                Ok(_) => return,
                Err(error) => {
                    eprintln!("(joy) input wait unavailable: {error}");
                    continue;
                }
            };
            let task = match server.tasks.get(&wait.task_id).await {
                Ok(Some(task)) => task,
                Ok(None) => return,
                Err(error) => {
                    eprintln!("(joy) input task unavailable: {error}");
                    continue;
                }
            };
            if task.status != joyczl_state::TaskStatus::Suspended
                || task.suspend_reason.as_deref() != Some(INPUT_WAIT_REASON)
            {
                let _ = rows.cancel(&wait.task_id).await;
                return;
            }
            match rows.expired(&wait.task_id).await {
                Ok(true) => {
                    if review_expired(&server, &sink, &wait).await {
                        return;
                    }
                    continue;
                }
                Ok(false) => (),
                Err(error) => {
                    eprintln!("(joy) input deadline unavailable: {error}");
                    continue;
                }
            }
            let condition = match rows.get(&wait.task_id).await {
                Ok(Some(condition)) if condition.event_id.is_some() => condition,
                Ok(_) => continue,
                Err(error) => {
                    eprintln!("(joy) input data unavailable: {error}");
                    continue;
                }
            };
            if server.resolved().is_none()
                || !server.active_turns(Some(&wait.session_id)).is_empty()
            {
                continue;
            }
            if resume_input(&server, &sink, current, &condition).await {
                return;
            }
        }
    });
}

async fn resume_input(
    server: &Server,
    sink: &EventSink,
    wait: joyczl_state::external_waits::InputWait,
    condition: &joyczl_state::external_waits::InputCondition,
) -> bool {
    let mut row = match server.checkpoints.get(&wait.turn_id).await {
        Ok(Some(row)) if row.status == "waiting_input" => row,
        _ => return false,
    };
    let mut messages: Vec<joyczl_provider::Message> = match serde_json::from_str(&row.messages_json)
    {
        Ok(messages) => messages,
        Err(error) => {
            crate::task_contract::stop_recovery(
                server,
                sink,
                &row,
                &format!("Input checkpoint is invalid: {error}"),
            )
            .await;
            return false;
        }
    };
    if let Err(error) = crate::task_recovery::require_known_messages(&messages) {
        crate::task_contract::stop_recovery(server, sink, &row, &error.message).await;
        return false;
    }
    if condition.condition_id != wait.condition_id
        || condition.task_id != wait.task_id
        || condition.event_id.is_none()
    {
        return false;
    }
    let note = match crate::task_inputs::input_message(condition) {
        Ok(note) => note,
        Err(error) => {
            crate::task_contract::stop_recovery(
                server,
                sink,
                &row,
                &format!("External input is invalid: {error}"),
            )
            .await;
            return false;
        }
    };
    messages.push(note);
    row.messages_json = match serde_json::to_string(&messages) {
        Ok(json) => json,
        Err(_) => return false,
    };
    crate::resume::resume_turn(server, sink, row, Some(crate::wake::Wake::Input(wait))).await
}

/// Expiry observations may arrive after a human control transition.
pub(crate) async fn review_expired(server: &Server, sink: &EventSink, wait: &InputWait) -> bool {
    let lock = server.session_write_lock(&format!("task-control:{}", wait.task_id));
    let _guard = lock.lock().await;
    let rows = ExternalWaits::new(server.pool.clone());
    let active = match rows.waiting(&wait.task_id).await {
        Ok(Some(current)) => {
            current.state == "waiting"
                && current.run_id == wait.run_id
                && current.turn_id == wait.turn_id
                && current.condition_id == wait.condition_id
        }
        Ok(None) => false,
        Err(error) => {
            eprintln!("(joy) cannot review input expiry: {error}");
            return false;
        }
    };
    if !active {
        return true;
    }
    match rows.expired(&wait.task_id).await {
        Ok(false) => return false,
        Ok(true) => (),
        Err(error) => {
            eprintln!("(joy) cannot verify input expiry: {error}");
            return false;
        }
    }
    let checkpoint = match server.checkpoints.get(&wait.turn_id).await {
        Ok(Some(cp))
            if cp.run_id.as_deref() == Some(wait.run_id.as_str())
                && cp.session_id == wait.session_id =>
        {
            cp
        }
        Ok(_) => return true,
        Err(error) => {
            eprintln!("(joy) input expiry checkpoint unavailable: {error}");
            return false;
        }
    };
    let reason = "Expected input did not arrive before its deadline; human review is required";
    // The shared review transaction preserves terminal state, latest-run identity and human pause fences.
    if let Err(error) = server
        .checkpoints
        .review(&checkpoint.turn_id, checkpoint.run_id.as_deref(), reason)
        .await
    {
        eprintln!("(joy) input expiry review could not be committed: {error}");
        return false;
    }
    if let Ok(Some(task)) = server.tasks.get(&wait.task_id).await {
        eprintln!(
            "(joy) task {}: input expiry reviewed, status {}",
            wait.task_id, task.status
        );
        crate::task::notify(sink, &task, task.last_stop_reason.as_deref());
    }
    if let Err(error) = rows.cancel(&wait.task_id).await {
        eprintln!("(joy) input expiry wait could not be closed: {error}");
        return false;
    }
    true
}
