//! Usage ledger (migration 0010): bookkeeping only, never enforcement.
//!
//! One row per turn; `summary` computes per-model aggregates in SQL. Deliberately no cost
//! column — under BYOK the money is on the user's own vendor bill; rates are the deployer's
//! business (tokens are stored; rates can be applied anytime). **In hub mode this is
//! per-user bookkeeping**: one home, one state.db per user; isolated per person by nature.

use anyhow::Result;
use sqlx::{FromRow, SqlitePool};

/// One aggregated row per model.
#[derive(Debug, Clone, FromRow)]
pub struct UsageByModel {
    pub provider: String,
    pub model: String,
    pub turns: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// Full summary.
#[derive(Debug, Clone, Default)]
pub struct UsageSummary {
    pub turns: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub by_model: Vec<UsageByModel>,
}

/// The usage ledger. Clone is cheap (one pool handle), same reason as Jobs.
#[derive(Clone)]
pub struct Usage {
    pool: SqlitePool,
}

impl Usage {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Record one turn's usage. Write failure **never blocks the turn** — bookkeeping is a side path.
    pub async fn record(
        &self,
        session_id: &str,
        provider: &str,
        model: &str,
        input_tokens: i64,
        output_tokens: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO usage_ledger (session_id, provider, model, input_tokens, output_tokens)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(session_id)
        .bind(provider)
        .bind(model)
        .bind(input_tokens)
        .bind(output_tokens)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Full summary (per-model aggregates + totals), by_model sorted by total input desc.
    pub async fn summary(&self) -> Result<UsageSummary> {
        let by_model = sqlx::query_as::<_, UsageByModel>(
            "SELECT provider, model, COUNT(*) AS turns,
                    SUM(input_tokens) AS input_tokens,
                    SUM(output_tokens) AS output_tokens
             FROM usage_ledger
             GROUP BY provider, model
             ORDER BY SUM(input_tokens) DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        let summary = UsageSummary {
            turns: by_model.iter().map(|row| row.turns).sum(),
            input_tokens: by_model.iter().map(|row| row.input_tokens).sum(),
            output_tokens: by_model.iter().map(|row| row.output_tokens).sum(),
            by_model,
        };
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> (Usage, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let pool = crate::open(&dir.path().join("state.db"))
            .await
            .expect("打开库");
        (Usage::new(pool), dir)
    }

    /// What goes in aggregates out; by_model sorted by input desc; empty table returns zeros.
    #[tokio::test]
    async fn records_and_aggregates_by_model() {
        let (usage, _dir) = db().await;
        let empty = usage.summary().await.expect("空汇总");
        assert_eq!(empty.turns, 0);

        usage
            .record("s1", "deepseek", "deepseek-chat", 100, 50)
            .await
            .expect("记一行");
        usage
            .record("s1", "deepseek", "deepseek-chat", 200, 80)
            .await
            .expect("再记一行");
        usage
            .record("s2", "openai", "gpt-5.2", 1000, 10)
            .await
            .expect("另一家");

        let summary = usage.summary().await.expect("汇总");
        assert_eq!(summary.turns, 3);
        assert_eq!(summary.input_tokens, 1300);
        assert_eq!(summary.output_tokens, 140);
        assert_eq!(summary.by_model.len(), 2);
        // Sorted by total input desc: gpt-5.2 (1000) first.
        assert_eq!(summary.by_model[0].model, "gpt-5.2");
        assert_eq!(summary.by_model[0].turns, 1);
        assert_eq!(summary.by_model[1].model, "deepseek-chat");
        assert_eq!(summary.by_model[1].turns, 2);
        assert_eq!(summary.by_model[1].input_tokens, 300);
    }
}
