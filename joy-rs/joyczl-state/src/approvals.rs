//! The approval audit ledger (migration 0011): what the model asked, and how it ended.
//!
//! Same discipline as the usage ledger (M2d): **write failures never block the main path** —
//! approval causality is guaranteed by the oneshot channel; the ledger is only a queryable
//! shadow. The four-state outcome (approved / denied / timeout / voided) comes from one place.

use anyhow::Result;
use sqlx::{FromRow, SqlitePool};

/// One audit row.
#[derive(Debug, Clone, FromRow)]
pub struct ApprovalLogRow {
    pub turn_id: String,
    pub request_id: String,
    pub tool: String,
    pub args_preview: String,
    pub reason: String,
    pub outcome: String,
    pub decided_at: String,
}

/// The approval audit ledger.
#[derive(Clone)]
pub struct Approvals {
    pool: SqlitePool,
}

impl Approvals {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Log one outcome. The outcome is the four-state string, produced by the caller — an
    /// enum would be over-design; this table's readers are humans and SQL.
    pub async fn log(
        &self,
        turn_id: &str,
        request_id: &str,
        tool: &str,
        args_preview: &str,
        reason: &str,
        outcome: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO approval_log (turn_id, request_id, tool, args_preview, reason, outcome)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(turn_id)
        .bind(request_id)
        .bind(tool)
        .bind(args_preview)
        .bind(reason)
        .bind(outcome)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Most recent records, newest first.
    pub async fn recent(&self, limit: i64) -> Result<Vec<ApprovalLogRow>> {
        let rows = sqlx::query_as::<_, ApprovalLogRow>(
            "SELECT turn_id, request_id, tool, args_preview, reason, outcome, decided_at
             FROM approval_log
             ORDER BY id DESC
             LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> (Approvals, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let pool = crate::open(&dir.path().join("state.db"))
            .await
            .expect("open the store");
        (Approvals::new(pool), dir)
    }

    /// All four outcomes persist; newest first; empty table returns empty.
    #[tokio::test]
    async fn the_log_records_all_four_outcomes() {
        let (log, _dir) = db().await;
        assert!(log.recent(10).await.expect("empty table").is_empty());

        for (i, outcome) in ["approved", "denied", "timeout", "voided"]
            .iter()
            .enumerate()
        {
            log.log(
                &format!("t{i}"),
                &format!("ap-{i}"),
                "run_command",
                "rm -rf /tmp/x",
                "the allowlist was not recorded",
                outcome,
            )
            .await
            .expect("record one row");
        }
        let rows = log.recent(10).await.expect("read back");
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].outcome, "voided", "newest first");
        assert_eq!(rows[3].outcome, "approved");
        assert!(rows[0].args_preview.contains("rm -rf"));
    }
}
