//! 情景记忆：发生过的事，带日期。

use anyhow::Result;
use sqlx::sqlite::SqlitePool;

use crate::fts::{like_pattern, to_match_expr};

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct EpisodeRow {
    pub id: i64,
    /// ISO 8601 日期。
    pub happened_at: String,
    pub summary: String,
    pub created_at: Option<String>,
}

#[derive(Clone)]
pub struct Episodes {
    pool: SqlitePool,
}

impl Episodes {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn add(&self, happened_at: &str, summary: &str) -> Result<i64> {
        let id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO episodes (happened_at, summary) VALUES (?, ?) RETURNING id",
        )
        .bind(happened_at)
        .bind(summary)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    /// 同 `Facts::search`：先 FTS5（trigram），没命中再退回 LIKE。
    pub async fn search(&self, query: &str, top_k: u32) -> Result<Vec<EpisodeRow>> {
        if let Some(expr) = to_match_expr(query) {
            let hits = sqlx::query_as::<_, EpisodeRow>(
                "SELECT e.id, e.happened_at, e.summary, e.created_at
                 FROM episodes_fts
                 JOIN episodes e ON e.id = episodes_fts.rowid
                 WHERE episodes_fts MATCH ?
                 ORDER BY bm25(episodes_fts)
                 LIMIT ?",
            )
            .bind(expr)
            .bind(top_k as i64)
            .fetch_all(&self.pool)
            .await?;
            if !hits.is_empty() {
                return Ok(hits);
            }
        }

        let Some(pattern) = like_pattern(query) else {
            return Ok(Vec::new());
        };
        sqlx::query_as::<_, EpisodeRow>(
            "SELECT id, happened_at, summary, created_at
             FROM episodes
             WHERE summary LIKE ? OR happened_at LIKE ?
             ORDER BY happened_at DESC, id DESC
             LIMIT ?",
        )
        .bind(&pattern)
        .bind(&pattern)
        .bind(top_k as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// 最近发生的事，不看查询词 —— 「我上次跟阿明聊了什么」这类问题靠它。
    pub async fn recent(&self, limit: u32) -> Result<Vec<EpisodeRow>> {
        sqlx::query_as::<_, EpisodeRow>(
            "SELECT id, happened_at, summary, created_at
             FROM episodes ORDER BY happened_at DESC, id DESC LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// 按编号删一条情景。触发器会同步清掉 FTS 索引。
    pub async fn delete(&self, id: i64) -> Result<bool> {
        let done = sqlx::query("DELETE FROM episodes WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }
}
