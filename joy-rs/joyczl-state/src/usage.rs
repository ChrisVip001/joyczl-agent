//! 用量台账（迁移 0010）：只记账，不强制。
//!
//! 每个 turn 一行；`summary` 用 SQL 现算 per-model 聚合。刻意没有成本列 ——
//! BYOK 模式下钱记在用户自己的厂商账单上，费率是部署方的事（token 在，
//! 费率随时能乘）。**在 hub 模式下这就是 per-user 记账**：每个用户一个
//! home、一个 state.db，台账天然按人隔离；跨用户汇总等有管理面再说。

use anyhow::Result;
use sqlx::{FromRow, SqlitePool};

/// 按模型聚合的一行。
#[derive(Debug, Clone, FromRow)]
pub struct UsageByModel {
    pub provider: String,
    pub model: String,
    pub turns: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// 全量汇总。
#[derive(Debug, Clone, Default)]
pub struct UsageSummary {
    pub turns: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub by_model: Vec<UsageByModel>,
}

/// 用量台账。Clone 便宜（一个连接池句柄），与 Jobs 同一条理由。
#[derive(Clone)]
pub struct Usage {
    pool: SqlitePool,
}

impl Usage {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 记一个 turn 的用量。写失败**绝不拦 turn** —— 记账是旁路，不是主路。
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

    /// 全量汇总（per-model 聚合 + 总数），by_model 按累计输入降序。
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

    /// 记进去的聚合得出来；by_model 按输入降序；空表返回零。
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
        // 按累计输入降序：gpt-5.2（1000）在前。
        assert_eq!(summary.by_model[0].model, "gpt-5.2");
        assert_eq!(summary.by_model[0].turns, 1);
        assert_eq!(summary.by_model[1].model, "deepseek-chat");
        assert_eq!(summary.by_model[1].turns, 2);
        assert_eq!(summary.by_model[1].input_tokens, 300);
    }
}
