//! Operation intent and observations commit with the checkpoint, never after execution advances.
use anyhow::{ensure, Result};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};

#[derive(Debug, Clone)]
pub struct OperationSnapshot {
    pub tool_use_id: String,
    pub tool: String,
    pub input_json: String,
    pub output: Option<String>,
}
#[derive(Debug, Clone, FromRow)]
pub struct OperationRow {
    pub id: String,
    pub task_id: Option<String>,
    pub run_id: Option<String>,
    pub turn_id: String,
    pub session_id: String,
    pub round_id: String,
    pub tool_use_id: String,
    pub tool: String,
    pub input_json: String,
    pub input_digest: String,
    pub origin: String,
    pub status: String,
    pub output_digest: Option<String>,
    pub evidence: Option<String>,
    pub started_at: String,
    pub settled_at: Option<String>,
}
#[derive(Clone, Copy)]
pub enum OperationOwner<'a> {
    Task(&'a str),
    Turn {
        session_id: &'a str,
        turn_id: &'a str,
    },
}
#[derive(Clone)]
pub struct Reconciliation<'a> {
    pub owner: OperationOwner<'a>,
    pub operation_id: &'a str,
    pub expected_checkpoint_digest: &'a str,
    pub resolution: &'a str,
    pub evidence: &'a str,
    pub messages_json: &'a str,
}
#[derive(Clone)]
pub struct ToolOperations {
    pool: SqlitePool,
}
impl ToolOperations {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
    pub async fn list(&self, task_id: &str) -> Result<Vec<OperationRow>> {
        Ok(
            sqlx::query_as("SELECT * FROM tool_operations WHERE task_id=? ORDER BY rowid")
                .bind(task_id)
                .fetch_all(&self.pool)
                .await?,
        )
    }
    /// Session inspection excludes task-owned operations and ordinary successful calls.
    pub async fn for_chat(&self, session: &str, turn: Option<&str>) -> Result<Vec<OperationRow>> {
        Ok(sqlx::query_as("SELECT o.* FROM tool_operations o JOIN turn_checkpoints c ON c.turn_id=o.turn_id WHERE o.task_id IS NULL AND o.run_id IS NULL AND o.session_id=? AND c.run_id IS NULL AND c.status IN ('suspended','abandoned') AND (? IS NULL OR o.turn_id=?) ORDER BY o.rowid")
            .bind(session).bind(turn).bind(turn).fetch_all(&self.pool).await?)
    }

    pub async fn get(&self, id: &str) -> Result<Option<OperationRow>> {
        Ok(sqlx::query_as("SELECT * FROM tool_operations WHERE id=?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }
    /// Recover metadata only from an unchanged stopped boundary; this does not prove transmission.
    pub async fn recover_intents(
        &self,
        row: &crate::CheckpointRow,
        operations: &[OperationSnapshot],
    ) -> Result<()> {
        let run = row.run_id.as_deref();
        let mut tx = self.pool.begin().await?;
        let changed=sqlx::query("UPDATE turn_checkpoints SET turn_id=turn_id WHERE turn_id=? AND run_id IS ? AND session_id=? AND status IN ('suspended','abandoned') AND messages_json=? AND round_id IS ?")
            .bind(&row.turn_id).bind(run).bind(&row.session_id).bind(&row.messages_json).bind(&row.round_id)
            .execute(&mut *tx).await?.rows_affected();
        ensure!(
            changed == 1,
            "Recovered checkpoint changed before operation inspection"
        );
        if let Some(run) = run {
            let eligible: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM task_runs r JOIN agent_tasks t ON t.id=r.task_id WHERE r.id=? AND t.status IN ('suspended','needs_attention') AND r.session_id=? AND (r.turn_id=? OR (r.turn_id IS NULL AND r.id=?)) AND r.id=(SELECT latest.id FROM task_runs latest WHERE latest.task_id=t.id ORDER BY latest.started_at DESC,latest.rowid DESC LIMIT 1))")
            .bind(run).bind(&row.session_id).bind(&row.turn_id).bind(&row.turn_id).fetch_one(&mut *tx).await?;
            ensure!(
                eligible,
                "Only the latest stopped execution can recover operation metadata"
            );
        }
        let fallback = format!("legacy-turn:{}", row.turn_id);
        checkpoint(
            &mut tx,
            &row.turn_id,
            &row.session_id,
            run,
            row.round_id.as_deref().unwrap_or(&fallback),
            "recovered_checkpoint",
            operations,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Human evidence settles uncertainty; it never executes a tool or resumes a task.
    pub async fn reconcile(&self, input: Reconciliation<'_>) -> Result<()> {
        ensure!(
            !input.evidence.trim().is_empty() && input.evidence.len() <= 16_384,
            "Reconciliation needs 1 to 16384 bytes of evidence"
        );
        ensure!(
            matches!(
                input.resolution,
                "reconciled_success" | "reconciled_failure" | "not_executed"
            ),
            "Invalid reconciliation outcome"
        );
        let mut tx = self.pool.begin().await?;
        // Acquire the writer before reading the checkpoint used by the compare-and-swap.
        let locked = sqlx::query("UPDATE tool_operations SET id=id WHERE id=?")
            .bind(input.operation_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        ensure!(locked == 1, "Operation is unavailable");
        let operation: OperationRow = sqlx::query_as("SELECT * FROM tool_operations WHERE id=?")
            .bind(input.operation_id)
            .fetch_one(&mut *tx)
            .await?;
        let owns = match input.owner {
            OperationOwner::Task(id) => operation.task_id.as_deref() == Some(id),
            OperationOwner::Turn {
                session_id,
                turn_id,
            } => {
                operation.task_id.is_none()
                    && operation.run_id.is_none()
                    && operation.session_id == session_id
                    && operation.turn_id == turn_id
            }
        };
        ensure!(owns, "Operation does not belong to this execution scope");
        if operation.status != "intent" {
            ensure!(
                operation.status == input.resolution
                    && operation.evidence.as_deref() == Some(input.evidence),
                "Operation already has a different immutable settlement"
            );
            tx.commit().await?;
            return Ok(());
        }
        match input.owner {
            OperationOwner::Task(task_id) => {
                let eligible: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM agent_tasks t JOIN task_runs r ON r.task_id=t.id JOIN turn_checkpoints c ON c.run_id=r.id WHERE t.id=? AND t.status IN ('suspended','needs_attention') AND r.id=? AND c.turn_id=? AND c.session_id=? AND c.status='suspended' AND (r.turn_id=c.turn_id OR (r.turn_id IS NULL AND r.id=c.turn_id)) AND r.id=(SELECT latest.id FROM task_runs latest WHERE latest.task_id=t.id ORDER BY latest.started_at DESC,latest.rowid DESC LIMIT 1))")
            .bind(task_id).bind(&operation.run_id).bind(&operation.turn_id).bind(&operation.session_id)
            .fetch_one(&mut *tx).await?;
                ensure!(
                    eligible,
                    "Only the stopped task's latest suspended execution can be reconciled"
                );
            }
            OperationOwner::Turn {
                session_id,
                turn_id,
            } => {
                let eligible: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM turn_checkpoints WHERE turn_id=? AND session_id=? AND run_id IS NULL AND status IN ('suspended','abandoned'))")
                    .bind(turn_id).bind(session_id).fetch_one(&mut *tx).await?;
                ensure!(
                    eligible,
                    "Only a stopped ordinary-chat checkpoint can be reconciled"
                );
            }
        }
        let original: String =
            sqlx::query_scalar("SELECT messages_json FROM turn_checkpoints WHERE turn_id=?")
                .bind(&operation.turn_id)
                .fetch_one(&mut *tx)
                .await?;
        ensure!(
            crate::ledger::digest_of(original.as_bytes()) == input.expected_checkpoint_digest,
            "Checkpoint changed; inspect the operation again before reconciling"
        );
        sqlx::query("UPDATE tool_operations SET status=?,evidence=?,settled_at=datetime('now') WHERE id=? AND status='intent'")
            .bind(input.resolution).bind(input.evidence).bind(input.operation_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE turn_checkpoints SET messages_json=?,updated_at=datetime('now') WHERE turn_id=? AND status IN ('suspended','abandoned')")
            .bind(input.messages_json).bind(&operation.turn_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
}

pub(crate) async fn checkpoint(
    tx: &mut Transaction<'_, Sqlite>,
    turn_id: &str,
    session_id: &str,
    run_id: Option<&str>,
    round_id: &str,
    origin: &str,
    operations: &[OperationSnapshot],
) -> Result<()> {
    ensure!(
        matches!(origin, "checkpoint" | "recovered_checkpoint"),
        "Invalid operation origin"
    );
    let mut identities = std::collections::HashSet::new();
    for operation in operations {
        ensure!(
            identities.insert(&operation.tool_use_id),
            "Duplicate tool call identity in one checkpoint"
        );
        ensure!(
            !operation.tool_use_id.is_empty() && !operation.tool.is_empty(),
            "Operation identity is empty"
        );
        let input: serde_json::Value = serde_json::from_str(&operation.input_json)?;
        let input_json = serde_json::to_string(&input)?;
        let digest = crate::ledger::digest_of(input_json.as_bytes());
        let identity = serde_json::to_vec(&(turn_id, round_id, &operation.tool_use_id))?;
        let id = crate::ledger::digest_of(&identity);
        sqlx::query("INSERT INTO tool_operations(id,task_id,run_id,turn_id,session_id,round_id,tool_use_id,tool,input_json,input_digest,origin) VALUES(?,(SELECT task_id FROM task_runs WHERE id=?),?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO NOTHING")
            .bind(&id).bind(run_id).bind(run_id).bind(turn_id).bind(session_id).bind(round_id)
            .bind(&operation.tool_use_id).bind(&operation.tool).bind(&input_json).bind(&digest).bind(origin)
            .execute(&mut **tx).await?;
        let existing: (Option<String>, String, String, String) = sqlx::query_as(
            "SELECT run_id,tool,input_digest,status FROM tool_operations WHERE id=?",
        )
        .bind(&id)
        .fetch_one(&mut **tx)
        .await?;
        ensure!(
            existing.0.as_deref() == run_id && existing.1 == operation.tool && existing.2 == digest,
            "Operation identity was reused with different execution or arguments"
        );
        if operation.output.is_none() {
            ensure!(
                existing.3 == "intent",
                "Settled operation identity cannot authorize another execution"
            );
        }
        if let Some(output) = &operation.output {
            let status = if output.starts_with("Error:") {
                "failed"
            } else {
                "succeeded"
            };
            let output_digest = crate::ledger::digest_of(output.as_bytes());
            let changed = sqlx::query("UPDATE tool_operations SET status=?,output_digest=?,settled_at=datetime('now') WHERE id=? AND status='intent'")
                .bind(status).bind(&output_digest).bind(&id).execute(&mut **tx).await?.rows_affected();
            if changed == 0 {
                let original: (String, Option<String>) =
                    sqlx::query_as("SELECT status,output_digest FROM tool_operations WHERE id=?")
                        .bind(&id)
                        .fetch_one(&mut **tx)
                        .await?;
                ensure!(
                    original.0 == status && original.1.as_deref() == Some(output_digest.as_str()),
                    "Settled operation cannot be replaced with another result"
                );
            }
        }
    }
    Ok(())
}
