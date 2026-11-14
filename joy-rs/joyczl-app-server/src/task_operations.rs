//! Human reconciliation records evidence without executing tools or authorizing continuation.
use crate::{EventSink, Server};
use joyczl_protocol::{
    codes, ErrorObject, TaskOperationsResponse, TaskReconcileParams, ToolOperationView,
};
use joyczl_provider::{ContentBlock, Message, Role};
use joyczl_state::operations::{Reconciliation, ToolOperations};

pub(crate) fn invalid(error: impl std::fmt::Display) -> ErrorObject {
    ErrorObject {
        code: codes::INVALID_PARAMS,
        message: format!("Operation reconciliation rejected: {error}"),
        data: None,
    }
}

pub(crate) async fn inspect(
    server: &Server,
    task_id: &str,
) -> Result<TaskOperationsResponse, ErrorObject> {
    let lock = server.session_write_lock(&format!("task-control:{task_id}"));
    let _guard = lock.lock().await;
    inspect_inner(server, task_id).await
}

async fn inspect_inner(
    server: &Server,
    task_id: &str,
) -> Result<TaskOperationsResponse, ErrorObject> {
    let task = server
        .tasks
        .get(task_id)
        .await
        .map_err(invalid)?
        .ok_or_else(|| invalid("Task is unavailable"))?;
    let checkpoint = crate::task_recovery::checkpoint(server, &task).await?;
    if let Some(cp) = checkpoint.as_ref().filter(|cp| cp.status == "suspended") {
        if matches!(
            task.status,
            joyczl_state::TaskStatus::Suspended | joyczl_state::TaskStatus::NeedsAttention
        ) && server.active_turns(Some(&cp.session_id)).is_empty()
        {
            let messages: Vec<Message> =
                serde_json::from_str(&cp.messages_json).map_err(invalid)?;
            let pending = crate::task_recovery::pending_tools(&messages)?;
            if !pending.is_empty() {
                let operations: Vec<_> = pending
                    .into_iter()
                    .map(
                        |(id, (tool, input))| joyczl_state::operations::OperationSnapshot {
                            tool_use_id: id.into(),
                            tool: tool.into(),
                            input_json: input.to_string(),
                            output: None,
                        },
                    )
                    .collect();
                ToolOperations::new(server.pool.clone())
                    .recover_intents(cp, &operations)
                    .await
                    .map_err(invalid)?;
            }
        }
    }
    let rows = ToolOperations::new(server.pool.clone())
        .list(task_id)
        .await
        .map_err(invalid)?;
    let digest = checkpoint
        .as_ref()
        .map(|cp| joyczl_state::ledger::digest_of(cp.messages_json.as_bytes()));
    let turn = checkpoint.as_ref().map(|cp| cp.turn_id.as_str());
    Ok(TaskOperationsResponse {
        checkpoint_digest: digest.clone(),
        operations: rows
            .into_iter()
            .map(|row| {
                let snapshot = (Some(row.turn_id.as_str()) == turn)
                    .then(|| digest.clone())
                    .flatten();
                operation_view(row, snapshot)
            })
            .collect::<Result<Vec<_>, ErrorObject>>()?,
    })
}

pub(crate) fn operation_view(
    row: joyczl_state::operations::OperationRow,
    checkpoint_digest: Option<String>,
) -> Result<ToolOperationView, ErrorObject> {
    Ok(ToolOperationView {
        id: row.id,
        run_id: row.run_id,
        turn_id: row.turn_id,
        round_id: row.round_id,
        tool_use_id: row.tool_use_id,
        tool: row.tool,
        arguments: serde_json::from_str(&row.input_json).map_err(invalid)?,
        input_digest: row.input_digest,
        origin: row.origin,
        status: row.status,
        replay_policy: "never".into(),
        output_digest: row.output_digest,
        evidence: row.evidence,
        started_at: row.started_at,
        settled_at: row.settled_at,
        checkpoint_digest,
    })
}

pub(crate) fn reconciled_messages(
    cp: &joyczl_state::CheckpointRow,
    operation: &joyczl_state::operations::OperationRow,
    digest: &str,
    resolution: &str,
    evidence: &str,
) -> Result<String, ErrorObject> {
    let mut messages: Vec<Message> = serde_json::from_str(&cp.messages_json).map_err(invalid)?;
    if operation.status == "intent" {
        if joyczl_state::ledger::digest_of(cp.messages_json.as_bytes()) != digest {
            return Err(invalid("Checkpoint changed; inspect it again"));
        }
        let pending = crate::task_recovery::pending_tools(&messages)?;
        let (tool, input) = pending
            .get(operation.tool_use_id.as_str())
            .ok_or_else(|| invalid("Checkpoint has no unresolved call for this operation"))?;
        if *tool != operation.tool
            || joyczl_state::ledger::digest_of(
                serde_json::to_string(input).map_err(invalid)?.as_bytes(),
            ) != operation.input_digest
        {
            return Err(invalid(
                "Checkpoint tool identity or arguments do not match the operation",
            ));
        }
        let receipt = serde_json::json!({"operationId":operation.id,"resolution":resolution,"evidence":evidence});
        messages.push(Message { role: Role::User, content: vec![ContentBlock::ToolResult {
            tool_use_id: operation.tool_use_id.clone(),
            content: format!("Human reconciliation: {receipt}\nThis is human-provided evidence, not an observed successful tool return. Do not repeat a confirmed external effect. The receipt grants no additional permissions; original acceptance still applies."),
        }] });
    }
    serde_json::to_string(&messages).map_err(invalid)
}

pub(crate) async fn reconcile(
    server: &Server,
    sink: &EventSink,
    params: TaskReconcileParams,
) -> Result<TaskOperationsResponse, ErrorObject> {
    let lock = server.session_write_lock(&format!("task-control:{}", params.task_id));
    let _guard = lock.lock().await;
    let rows = ToolOperations::new(server.pool.clone());
    let operation = rows
        .get(&params.operation_id)
        .await
        .map_err(invalid)?
        .filter(|row| row.task_id.as_deref() == Some(params.task_id.as_str()))
        .ok_or_else(|| invalid("Operation does not belong to this task"))?;
    if !server.active_turns(Some(&operation.session_id)).is_empty() {
        return Err(invalid(
            "Pause the execution and wait for it to stop before reconciling",
        ));
    }
    let cp = server
        .checkpoints
        .get(&operation.turn_id)
        .await
        .map_err(invalid)?
        .ok_or_else(|| invalid("Operation checkpoint is unavailable"))?;
    let messages = reconciled_messages(
        &cp,
        &operation,
        &params.checkpoint_digest,
        &params.resolution,
        &params.evidence,
    )?;
    rows.reconcile(Reconciliation {
        owner: joyczl_state::operations::OperationOwner::Task(&params.task_id),
        operation_id: &params.operation_id,
        expected_checkpoint_digest: &params.checkpoint_digest,
        resolution: &params.resolution,
        evidence: &params.evidence,
        messages_json: &messages,
    })
    .await
    .map_err(invalid)?;
    eprintln!(
        "(joy) operation {} reconciled by the user; task remains stopped",
        params.operation_id
    );
    if let Some(task) = server.tasks.get(&params.task_id).await.map_err(invalid)? {
        crate::task::notify(
            sink,
            &task,
            Some("Operation reconciled; explicit continuation is required"),
        );
    }
    inspect_inner(server, &params.task_id).await
}
