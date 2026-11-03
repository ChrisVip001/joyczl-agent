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

    /// Job finish. `status`: `completed` / `failed` / `killed`.
    pub async fn record_finished(
        &self,
        id: &str,
        status: &str,
        exit_code: Option<i32>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE jobs SET status = ?, exit_code = ?, finished_at = datetime('now') WHERE id = ?",
        )
        .bind(status)
        .bind(exit_code)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Startup reconciliation: mark still-`running` rows (leftovers of the last crash) as
    /// `interrupted`, returning them — the caller logs "what happened" row by row.
    pub async fn reconcile_interrupted(&self) -> Result<Vec<JobRow>> {
        let rows = sqlx::query_as::<_, JobRow>(
            "SELECT id, owner, command, status, exit_code, log_path, started_at, finished_at
             FROM jobs WHERE status = 'running'",
        )
        .fetch_all(&self.pool)
        .await?;
        sqlx::query(
            "UPDATE jobs SET status = 'interrupted', finished_at = datetime('now')
             WHERE status = 'running'",
        )
        .execute(&self.pool)
        .await?;
        Ok(rows)
    }

    /// An owner's past jobs (crash leftovers included), newest first.
    pub async fn recent(&self, owner: &str, limit: u32) -> Result<Vec<JobRow>> {
        Ok(sqlx::query_as::<_, JobRow>(
            "SELECT id, owner, command, status, exit_code, log_path, started_at, finished_at
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
            "SELECT id, owner, command, status, exit_code, log_path, started_at, finished_at
             FROM jobs WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }
}
