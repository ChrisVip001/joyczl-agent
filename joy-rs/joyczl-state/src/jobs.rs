//! The ledger of background jobs (migration 0009).
//!
//! Only **facts** are recorded here (what ran, status, exit code, where output is); no
//! scheduling, no reruns. Startup reconciliation marks still-`running` rows as
//! `interrupted`, outputs stay — reruns are decided by human/model reading the records.

use anyhow::Result;
use sqlx::{FromRow, SqlitePool};

/// One row of the ledger.
#[derive(Debug, Clone, FromRow)]
pub struct JobRow {
    pub run_id: Option<String>,
    pub notify_target: Option<String>,
    pub task_id: Option<String>,
    pub id: String,
    pub owner: String,
    pub command: String,
    /// `running` / `completed` / `failed` / `killed` / `interrupted`.
    pub status: String,
    pub exit_code: Option<i32>,
    pub log_path: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

/// The job ledger. Clone is cheap (one pool handle) — callers and tests often want two.
#[derive(Clone)]
pub struct Jobs {
    pool: SqlitePool,
}

impl Jobs {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Random durable identity; independent of process counters and restarts.
    pub async fn new_id(&self) -> Result<String> {
        Ok(
            sqlx::query_scalar("SELECT 'j-' || lower(hex(randomblob(16)))")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// Job start: record one `running` row (started_at filled by the DB default).
    pub async fn record_started(
        &self,
        id: &str,
        owner: &str,
        command: &str,
        log_path: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO jobs (id, owner, command, status, log_path)
             VALUES (?, ?, ?, 'running', ?)",
        )
        .bind(id)
        .bind(owner)
        .bind(command)
        .bind(log_path)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Bind using the host's exact execution identity, never the session's newest task.
    pub async fn record_started_for_run(
        &self,
        id: &str,
        owner: &str,
        command: &str,
        log_path: &str,
        run_id: Option<&str>,
    ) -> Result<()> {
        self.record_started_with_delivery(id, owner, command, log_path, run_id, false, None)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn record_started_with_delivery(
        &self,
        id: &str,
        owner: &str,
        command: &str,
        log_path: &str,
        run_id: Option<&str>,
        notify_requested: bool,
        notify_target: Option<&str>,
    ) -> Result<()> {
        let result = if let Some(run_id) = run_id {
            sqlx::query("INSERT INTO jobs (id, owner, command, status, log_path, run_id, task_id, notify_requested, notify_target)
                SELECT ?, ?, ?, 'running', ?, id, task_id, ?, ? FROM task_runs WHERE id = ? AND session_id = ? AND status = 'running'")
                .bind(id).bind(owner).bind(command).bind(log_path).bind(notify_requested).bind(notify_target).bind(run_id).bind(owner).execute(&self.pool).await?
        } else {
            sqlx::query("INSERT INTO jobs (id, owner, command, status, log_path, notify_requested, notify_target) VALUES (?, ?, ?, 'running', ?, ?, ?)")
                .bind(id).bind(owner).bind(command).bind(log_path).bind(notify_requested).bind(notify_target).execute(&self.pool).await?
        };
        anyhow::ensure!(
            result.rows_affected() == 1,
            "job execution identity is unavailable or belongs to another session"
        );
        Ok(())
    }

    /// Job finish. `status`: `completed` / `failed` / `killed`.
    pub async fn record_finished(
        &self,
        id: &str,
        status: &str,
        exit_code: Option<i32>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query("UPDATE jobs SET status = ?, exit_code = ?, finished_at = datetime('now') WHERE id = ? AND status = 'running'")
            .bind(status).bind(exit_code).bind(id).execute(&mut *tx).await?.rows_affected();
        if updated == 1 {
            sqlx::query("INSERT INTO job_completions (job_id) VALUES (?)")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO task_events (task_id, run_id, seq, kind, detail)
                SELECT j.task_id, j.run_id, (SELECT COALESCE(MAX(seq), 0) + 1 FROM task_events WHERE task_id = j.task_id), 'job_finished', ?
                FROM jobs j WHERE j.id = ? AND EXISTS (SELECT 1 FROM agent_tasks WHERE id = j.task_id)")
                .bind(format!("{id}: {status}, exit {exit_code:?}")).bind(id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Pending notification production, already committed with the job outcome.
    pub async fn pending_notifications(&self) -> Result<Vec<JobRow>> {
        Ok(sqlx::query_as("SELECT j.id, j.owner, j.command, j.status, j.exit_code, j.log_path, j.started_at, j.finished_at, j.run_id, j.task_id, j.notify_target
            FROM jobs j JOIN job_completions c ON c.job_id = j.id WHERE j.notify_requested = 1 AND c.notification_enqueued = 0 ORDER BY c.created_at, j.id LIMIT 100")
            .fetch_all(&self.pool).await?)
    }

    pub async fn notification_enqueued(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE job_completions SET notification_enqueued = 1 WHERE job_id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Startup reconciliation: mark still-`running` rows (leftovers of the last crash) as
    /// `interrupted`, returning them — the caller logs "what happened" row by row.
    pub async fn reconcile_interrupted(&self) -> Result<Vec<JobRow>> {
        let rows = sqlx::query_as::<_, JobRow>(
            "SELECT id, owner, command, status, exit_code, log_path, started_at, finished_at, run_id, task_id, notify_target
             FROM jobs WHERE status = 'running'",
        )
        .fetch_all(&self.pool)
        .await?;
        for row in &rows {
            self.record_finished(&row.id, "interrupted", None).await?;
        }
        Ok(rows)
    }

    pub async fn for_task(&self, task_id: &str) -> Result<Vec<JobRow>> {
        Ok(sqlx::query_as("SELECT id, owner, command, status, exit_code, log_path, started_at, finished_at, run_id, task_id, notify_target FROM jobs WHERE task_id = ? ORDER BY started_at, id")
            .bind(task_id).fetch_all(&self.pool).await?)
    }

    /// An owner's past jobs (crash leftovers included), newest first.
    pub async fn recent(&self, owner: &str, limit: u32) -> Result<Vec<JobRow>> {
        Ok(sqlx::query_as::<_, JobRow>(
            "SELECT id, owner, command, status, exit_code, log_path, started_at, finished_at, run_id, task_id, notify_target
             FROM jobs WHERE owner = ?
             ORDER BY started_at DESC LIMIT ?",
        )
        .bind(owner)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Fetch one row by id (owner fencing is checked by the caller).
    pub async fn get(&self, id: &str) -> Result<Option<JobRow>> {
        Ok(sqlx::query_as::<_, JobRow>(
            "SELECT id, owner, command, status, exit_code, log_path, started_at, finished_at, run_id, task_id, notify_target
             FROM jobs WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }
}
