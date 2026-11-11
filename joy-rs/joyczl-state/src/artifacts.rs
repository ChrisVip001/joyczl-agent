//! The artifact list (migration 0017).
//!
//! "What did this task actually produce?" answered with files, not with the model's summary of
//! them. One row per path per task: writing the same file twice updates its size instead of
//! stacking duplicates — the list is a deliverable list, not an append-only log.

use anyhow::Result;
use sqlx::{FromRow, SqlitePool};

/// One delivered file.
#[derive(Debug, Clone, FromRow)]
pub struct ArtifactRow {
    pub id: i64,
    pub task_id: String,
    pub run_id: Option<String>,
    /// Absolute path as written. The dashboard shows it, never opens it.
    pub path: String,
    pub bytes: i64,
    pub at: String,
}

/// The artifact store. Clone is cheap (one pool handle).
#[derive(Clone)]
pub struct Artifacts {
    pool: SqlitePool,
}

impl Artifacts {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Record (or refresh) one delivered file.
    pub async fn add(
        &self,
        task_id: &str,
        run_id: Option<&str>,
        path: &str,
        bytes: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO task_artifacts (task_id, run_id, path, bytes) VALUES (?, ?, ?, ?)
             ON CONFLICT(task_id, path) DO UPDATE SET
                 run_id = excluded.run_id,
                 bytes = excluded.bytes,
                 at = datetime('now')",
        )
        .bind(task_id)
        .bind(run_id)
        .bind(path)
        .bind(bytes.max(0))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Newest first — the card shows the freshest five.
    pub async fn list(&self, task_id: &str, limit: i64) -> Result<Vec<ArtifactRow>> {
        Ok(sqlx::query_as::<_, ArtifactRow>(
            "SELECT id, task_id, run_id, path, bytes, at FROM task_artifacts
             WHERE task_id = ? ORDER BY id DESC LIMIT ?",
        )
        .bind(task_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }
}
