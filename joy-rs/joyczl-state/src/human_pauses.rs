//! Human continuation is the only path that clears a durable pause fence.
use anyhow::Result;
use sqlx::SqlitePool;

#[derive(Clone)]
pub struct HumanPauses {
    pool: SqlitePool,
}
impl HumanPauses {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
    pub async fn is_paused(&self, task_id: &str) -> Result<bool> {
        Ok(
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM task_pause_fences WHERE task_id=?)")
                .bind(task_id)
                .fetch_one(&self.pool)
                .await?,
        )
    }
    /// Commit human authorization, task activation and its recovery boundary together.
    pub async fn resume_checkpoint(
        &self,
        task_id: &str,
        turn_id: &str,
        messages_json: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query("UPDATE agent_tasks SET status='running',current_turn_id=?,suspend_reason=NULL,updated_at=datetime('now') WHERE id=? AND status IN ('suspended','needs_attention')")
            .bind(turn_id).bind(task_id).execute(&mut *tx).await?.rows_affected();
        anyhow::ensure!(changed == 1, "Task changed before human continuation");
        // Explicit human continuation can reopen a stopped latest checkpoint; cancellation stays fenced.
        let changed = sqlx::query("UPDATE turn_checkpoints SET status='open',messages_json=?,updated_at=datetime('now') WHERE turn_id=? AND status IN ('open','suspended','waiting_jobs','waiting_input','done','abandoned') AND run_id=(SELECT id FROM task_runs WHERE task_id=? ORDER BY started_at DESC,rowid DESC LIMIT 1)")
            .bind(messages_json).bind(turn_id).bind(task_id).execute(&mut *tx).await?.rows_affected();
        anyhow::ensure!(changed == 1, "Checkpoint changed before human continuation");
        sqlx::query("DELETE FROM task_pause_fences WHERE task_id=?")
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    /// Host-only human authorization; a model must never receive this operation as a tool.
    pub async fn authorize_resume(&self, task_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM task_pause_fences WHERE task_id=?")
            .bind(task_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
