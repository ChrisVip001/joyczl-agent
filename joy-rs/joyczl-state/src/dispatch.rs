//! Durable initial task dispatch. Unknown claimed work is never submitted again.
use anyhow::{ensure, Result};
use sqlx::{FromRow, SqlitePool};

#[derive(Clone, Debug, FromRow)]
pub struct DispatchRow {
    pub task_id: String,
    pub session_id: String,
    pub turn_id: String,
    pub message: String,
    pub state: String,
}
#[derive(FromRow)]
struct ProgressRow {
    id: String,
    title: String,
    status: String,
    last_stop_reason: Option<String>,
    acceptance_json: String,
}
#[derive(Clone)]
pub struct TaskDispatch {
    pool: SqlitePool,
}
impl TaskDispatch {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
    pub async fn enqueue(&self, task_id: &str, message: &str) -> Result<DispatchRow> {
        ensure!(
            !message.trim().is_empty() && message.len() <= 64_000,
            "task message must contain 1 to 64000 bytes"
        );
        let row=sqlx::query_as::<_,DispatchRow>("INSERT INTO task_dispatch(task_id,session_id,turn_id,message)
            SELECT id, 'task-work:' || lower(hex(randomblob(16))), 'task-turn:' || lower(hex(randomblob(16))), ?
            FROM agent_tasks WHERE id=? AND status='pending' AND current_turn_id IS NULL
            RETURNING task_id,session_id,turn_id,message,state")
            .bind(message).bind(task_id).fetch_optional(&self.pool).await?;
        row.ok_or_else(|| {
            anyhow::anyhow!("only a pending task without an execution can be started")
        })
    }
    pub async fn get(&self, task_id: &str) -> Result<Option<DispatchRow>> {
        Ok(sqlx::query_as(
            "SELECT task_id,session_id,turn_id,message,state FROM task_dispatch WHERE task_id=?",
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?)
    }
    pub async fn claim(&self) -> Result<Option<DispatchRow>> {
        Ok(sqlx::query_as(
            "UPDATE task_dispatch SET state='claimed' WHERE task_id=(
            SELECT d.task_id FROM task_dispatch d JOIN agent_tasks t ON t.id=d.task_id
            WHERE d.state='pending' AND t.status='pending' AND NOT EXISTS(SELECT 1 FROM task_pause_fences p WHERE p.task_id=t.id) ORDER BY d.rowid LIMIT 1)
            AND NOT EXISTS(SELECT 1 FROM task_dispatch WHERE state='claimed')
            RETURNING task_id,session_id,turn_id,message,state",
        )
        .fetch_optional(&self.pool)
        .await?)
    }
    /// Bounded, owner-scoped progress without exposing another task's working transcript.
    pub async fn progress(&self, owner_session: &str) -> Result<Vec<serde_json::Value>> {
        let rows: Vec<ProgressRow> = sqlx::query_as(
            "SELECT t.id,t.title,t.status,t.last_stop_reason,t.acceptance_json
             FROM agent_tasks t JOIN task_dispatch d ON d.task_id=t.id WHERE t.session_id=? ORDER BY d.rowid DESC LIMIT 5")
            .bind(owner_session).fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let status = if row.status == "done" && row.acceptance_json == "[]" {
                    "produced_unverified".to_string()
                } else {
                    row.status
                };
                serde_json::json!({
                    "id":row.id,
                    "title":row.title.chars().take(200).collect::<String>(),
                    "status":status,
                    "reason":row.last_stop_reason.map(|r|r.chars().take(500).collect::<String>())
                })
            })
            .collect())
    }

    /// A human pause before checkpoint admission may return the untouched dispatch to pending.
    pub async fn release_unstarted(&self, task_id: &str) -> Result<bool> {
        let changed=sqlx::query("UPDATE task_dispatch SET state='pending' WHERE task_id=? AND state='claimed'
            AND NOT EXISTS(SELECT 1 FROM turn_checkpoints c WHERE c.turn_id=task_dispatch.turn_id)
            AND EXISTS(SELECT 1 FROM agent_tasks t WHERE t.id=task_dispatch.task_id AND t.status IN ('pending','suspended') AND t.current_turn_id IS NULL)")
            .bind(task_id).execute(&self.pool).await?.rows_affected();
        Ok(changed == 1)
    }
    pub async fn resume_unstarted(&self, task_id: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let changed=sqlx::query("UPDATE agent_tasks SET status='pending',current_turn_id=NULL,suspend_reason=NULL,updated_at=datetime('now')
            WHERE id=? AND status IN ('suspended','needs_attention')
            AND EXISTS(SELECT 1 FROM task_dispatch d WHERE d.task_id=agent_tasks.id
                AND NOT EXISTS(SELECT 1 FROM turn_checkpoints c WHERE c.turn_id=d.turn_id)
                AND NOT EXISTS(SELECT 1 FROM task_runs r WHERE r.id=d.turn_id))")
            .bind(task_id).execute(&mut *tx).await?.rows_affected();
        if changed == 1 {
            sqlx::query("DELETE FROM task_pause_fences WHERE task_id=?")
                .bind(task_id)
                .execute(&mut *tx)
                .await?;

            sqlx::query("UPDATE task_dispatch SET state=CASE WHEN state IN ('finished','interrupted') THEN 'pending' ELSE state END,finished_at=NULL WHERE task_id=?")
                .bind(task_id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(changed == 1)
    }
    pub async fn finish(&self, task_id: &str) -> Result<()> {
        sqlx::query("UPDATE task_dispatch SET state='finished',finished_at=datetime('now') WHERE task_id=? AND state='claimed'")
            .bind(task_id).execute(&self.pool).await?;
        Ok(())
    }
    /// Checkpoint recovery owns dispatched work. A claim without a checkpoint needs review.
    pub async fn reconcile(&self) -> Result<Vec<String>> {
        let mut tx = self.pool.begin().await?;
        let missing: Vec<String> = sqlx::query_scalar(
            "SELECT task_id FROM task_dispatch d WHERE state='claimed'
            AND NOT EXISTS(SELECT 1 FROM turn_checkpoints c WHERE c.turn_id=d.turn_id)",
        )
        .fetch_all(&mut *tx)
        .await?;
        for id in &missing {
            sqlx::query("UPDATE agent_tasks SET status='needs_attention',suspend_reason='Task dispatch was interrupted before its checkpoint; review before retrying',updated_at=datetime('now') WHERE id=? AND status IN ('pending','running','suspended')")
                .bind(id).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE task_dispatch SET state='interrupted' WHERE state='claimed'")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(missing)
    }
}
