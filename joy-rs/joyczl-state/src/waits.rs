//! Durable waits for background jobs belonging to an already authorized task.
use anyhow::Result;
use sqlx::SqlitePool;

pub const JOB_WAIT_REASON: &str = "waiting for background jobs";
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct JobWait {
    pub task_id: String,
    pub run_id: String,
    pub turn_id: String,
    pub session_id: String,
    pub state: String,
}
#[derive(Clone)]
pub struct JobWaits {
    pool: SqlitePool,
}
impl JobWaits {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
    pub async fn register(
        &self,
        task_id: &str,
        run_id: &str,
        turn_id: &str,
        session_id: &str,
        job_ids: &[String],
    ) -> Result<()> {
        anyhow::ensure!(
            !job_ids.is_empty(),
            "a job wait must identify its dependencies"
        );
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query("UPDATE agent_tasks SET status = 'suspended', suspend_reason = ?, last_stop_reason = ?, updated_at = datetime('now')
            WHERE id = ? AND current_turn_id = ? AND status = 'running'")
            .bind(JOB_WAIT_REASON).bind(JOB_WAIT_REASON).bind(task_id).bind(turn_id).execute(&mut *tx).await?.rows_affected();
        anyhow::ensure!(updated == 1, "task no longer authorizes this wait");
        let checkpoint = sqlx::query("UPDATE turn_checkpoints SET status = 'waiting_jobs' WHERE turn_id = ? AND run_id = ? AND session_id = ? AND status IN ('open', 'done', 'waiting_jobs')")
            .bind(turn_id).bind(run_id).bind(session_id).execute(&mut *tx).await?.rows_affected();
        anyhow::ensure!(checkpoint == 1, "job wait has no recoverable checkpoint");
        let execution = sqlx::query("UPDATE task_runs SET status = 'waiting', stop_reason = ? WHERE id = ? AND task_id = ? AND session_id = ? AND status = 'running'")
            .bind(JOB_WAIT_REASON).bind(run_id).bind(task_id).bind(session_id).execute(&mut *tx).await?.rows_affected();
        anyhow::ensure!(
            execution == 1,
            "task wait execution identity is no longer active"
        );
        sqlx::query("UPDATE task_runs SET status = 'suspended', stop_reason = 'superseded by a later task turn' WHERE id IN (SELECT run_id FROM task_job_waits WHERE task_id = ? AND run_id != ?) AND status = 'waiting'")
            .bind(task_id).bind(run_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE turn_checkpoints SET status = 'suspended' WHERE turn_id IN (SELECT turn_id FROM task_job_waits WHERE task_id = ? AND turn_id != ?) AND status = 'waiting_jobs'")
            .bind(task_id).bind(turn_id).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO task_job_waits (task_id, run_id, turn_id, session_id) VALUES (?, ?, ?, ?)
            ON CONFLICT(task_id) DO UPDATE SET run_id = excluded.run_id, turn_id = excluded.turn_id, session_id = excluded.session_id, state = 'waiting', created_at = datetime('now')")
            .bind(task_id).bind(run_id).bind(turn_id).bind(session_id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM task_job_wait_members WHERE task_id = ?")
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
        for job in job_ids {
            let inserted = sqlx::query("INSERT INTO task_job_wait_members (task_id, job_id) SELECT ?, id FROM jobs WHERE id = ? AND task_id = ?")
                .bind(task_id).bind(job).bind(task_id).execute(&mut *tx).await?.rows_affected();
            anyhow::ensure!(
                inserted == 1,
                "job dependency is unavailable or belongs to another task"
            );
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn jobs(&self, task_id: &str) -> Result<Vec<crate::JobRow>> {
        Ok(sqlx::query_as("SELECT j.id, j.owner, j.command, j.status, j.exit_code, j.log_path, j.started_at, j.finished_at, j.run_id, j.task_id, j.notify_target
            FROM jobs j JOIN task_job_wait_members m ON m.job_id = j.id WHERE m.task_id = ? ORDER BY j.started_at, j.id")
            .bind(task_id).fetch_all(&self.pool).await?)
    }
    pub async fn pending(&self) -> Result<Vec<JobWait>> {
        Ok(sqlx::query_as("SELECT task_id, run_id, turn_id, session_id, state FROM task_job_waits WHERE state = 'waiting' ORDER BY created_at")
            .fetch_all(&self.pool).await?)
    }
    pub async fn get(&self, task_id: &str) -> Result<Option<JobWait>> {
        Ok(sqlx::query_as("SELECT task_id, run_id, turn_id, session_id, state FROM task_job_waits WHERE task_id = ?")
            .bind(task_id).fetch_optional(&self.pool).await?)
    }
    pub async fn cancel(&self, task_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE task_job_waits SET state = 'cancelled' WHERE task_id = ? AND state = 'waiting'",
        )
        .bind(task_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    /// Called after the session gate and task-control lock are held. A human pause/cancel
    /// changing the task state wins, and concurrent wake attempts can claim only once.
    pub async fn claim(&self, wait: &JobWait, messages_json: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let claimed = sqlx::query("UPDATE task_job_waits SET state = 'claimed' WHERE task_id = ? AND run_id = ? AND turn_id = ? AND state = 'waiting'
            AND NOT EXISTS(SELECT 1 FROM task_pause_fences p WHERE p.task_id=task_job_waits.task_id)
            AND EXISTS (SELECT 1 FROM agent_tasks t WHERE t.id = task_job_waits.task_id AND t.status = 'suspended' AND t.suspend_reason = ? AND t.current_turn_id = task_job_waits.turn_id)
            AND EXISTS (SELECT 1 FROM task_job_wait_members WHERE task_id = task_job_waits.task_id)
            AND NOT EXISTS (SELECT 1 FROM jobs j JOIN task_job_wait_members m ON m.job_id = j.id WHERE m.task_id = task_job_waits.task_id AND j.status IN ('running', 'interrupted', 'killed'))")
            .bind(&wait.task_id).bind(&wait.run_id).bind(&wait.turn_id).bind(JOB_WAIT_REASON).execute(&mut *tx).await?.rows_affected();
        if claimed == 0 {
            tx.rollback().await?;
            return Ok(false);
        }
        sqlx::query(
            "UPDATE agent_tasks SET status = 'running', suspend_reason = NULL WHERE id = ?",
        )
        .bind(&wait.task_id)
        .execute(&mut *tx)
        .await?;
        let checkpoint = sqlx::query("UPDATE turn_checkpoints SET status = 'open', messages_json = ? WHERE turn_id = ? AND status = 'waiting_jobs'")
            .bind(messages_json).bind(&wait.turn_id).execute(&mut *tx).await?.rows_affected();
        anyhow::ensure!(
            checkpoint == 1,
            "waiting checkpoint was changed before admission"
        );
        tx.commit().await?;
        Ok(true)
    }
}
