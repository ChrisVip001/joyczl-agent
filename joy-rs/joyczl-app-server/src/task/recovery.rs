//! Admission uses exact task/run lineage even after attention clears the live heartbeat.
use crate::Server;
use joyczl_protocol::{codes, ErrorObject};
use joyczl_provider::{ContentBlock, Message};
use joyczl_state::{CheckpointRow, TaskRow};
use std::collections::BTreeMap;

fn blocked(error: impl std::fmt::Display) -> ErrorObject {
    ErrorObject {
        code: codes::INVALID_PARAMS,
        message: format!("Cannot continue task; reconcile its checkpoint first: {error}"),
        data: None,
    }
}

pub(crate) async fn checkpoint(
    server: &Server,
    task: &TaskRow,
) -> Result<Option<CheckpointRow>, ErrorObject> {
    let run = server
        .runs
        .list_by_task(&task.id, 1)
        .await
        .map_err(blocked)?
        .into_iter()
        .next();
    let turn_id = task.current_turn_id.as_deref().or_else(|| {
        run.as_ref()
            .map(|run| run.turn_id.as_deref().unwrap_or(&run.id))
    });
    let Some(turn_id) = turn_id else {
        return Ok(None);
    };
    let checkpoint = server.checkpoints.get(turn_id).await.map_err(blocked)?;
    if let Some(checkpoint) = &checkpoint {
        let Some(run) = run else {
            return Err(blocked("checkpoint has no task execution identity"));
        };
        if checkpoint.run_id.as_deref() != Some(run.id.as_str()) {
            return Err(blocked("checkpoint does not match the task's latest run"));
        }
    }
    Ok(checkpoint)
}

pub(crate) fn require_known(checkpoint: &CheckpointRow) -> Result<(), ErrorObject> {
    let messages: Vec<Message> =
        serde_json::from_str(&checkpoint.messages_json).map_err(blocked)?;
    require_known_messages(&messages)
}

pub(crate) fn require_known_messages(messages: &[Message]) -> Result<(), ErrorObject> {
    let pending = pending_tools(messages)?;
    if !pending.is_empty() {
        return Err(blocked(
            "tool outcomes are unknown; ordinary resume does not confirm their external effects",
        ));
    }
    Ok(())
}

/// Pair call IDs chronologically; completed calls from earlier rounds are not uncertainty.
pub(crate) fn pending_tools(
    messages: &[Message],
) -> Result<BTreeMap<&str, (&str, &serde_json::Value)>, ErrorObject> {
    let mut pending = BTreeMap::new();
    for message in messages {
        for block in &message.content {
            match block {
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => {
                    if pending
                        .insert(id.as_str(), (name.as_str(), input))
                        .is_some()
                    {
                        return Err(blocked("duplicate unresolved tool call identity"));
                    }
                }
                ContentBlock::ToolResult { tool_use_id, .. } => {
                    pending.remove(tool_use_id.as_str());
                }
                _ => (),
            }
        }
    }
    Ok(pending)
}
