//! 审批审计台账（迁移 0011）：模型问过什么、结果如何。
//!
//! 与用量台账（M2d）同一条纪律：**写失败不拦主路** —— 审批的因果由
//! oneshot 通道保证，台账只是事后可查的影子。四态 outcome（approved /
//! denied / timeout / voided）都从等待 future 的收尾处出，写入口唯一。

use anyhow::Result;
use sqlx::{FromRow, SqlitePool};

/// 审计行。
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

/// 审批审计台账。
#[derive(Clone)]
pub struct Approvals {
    pool: SqlitePool,
}

impl Approvals {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 记一条结果。outcome 就是四态字符串，由调用方出 —— 拆枚举是过度设计，
    /// 这张表的读者是人和 SQL。
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

    /// 最近的记录，新的在前。
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
        let dir = tempfile::tempdir().expect("临时目录");
        let pool = crate::open(&dir.path().join("state.db"))
            .await
            .expect("打开库");
        (Approvals::new(pool), dir)
    }

    /// 四态都落得进去；新的在前；空表返回空。
    #[tokio::test]
    async fn the_log_records_all_four_outcomes() {
        let (log, _dir) = db().await;
        assert!(log.recent(10).await.expect("空表").is_empty());

        for (i, outcome) in ["approved", "denied", "timeout", "voided"]
            .iter()
            .enumerate()
        {
            log.log(
                &format!("t{i}"),
                &format!("ap-{i}"),
                "run_command",
                "rm -rf /tmp/x",
                "放行表没盖住",
                outcome,
            )
            .await
            .expect("记一行");
        }
        let rows = log.recent(10).await.expect("读回");
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].outcome, "voided", "新的在前");
        assert_eq!(rows[3].outcome, "approved");
        assert!(rows[0].args_preview.contains("rm -rf"));
    }
}
