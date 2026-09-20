//! 语义记忆：关于用户、关于人、关于项目的长期事实。

use anyhow::Result;
use sqlx::sqlite::SqlitePool;

use crate::fts::{like_pattern, to_match_expr};

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct FactRow {
    pub id: i64,
    pub subject: String,
    pub content: String,
    /// `user` = 用户直接说的；`consolidation` = 从对话里提炼的。
    pub source: String,
    pub created_at: Option<String>,
}

#[derive(Clone)]
pub struct Facts {
    pool: SqlitePool,
}

impl Facts {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 写一条事实，返回写进去的完整行（含 id 和 created_at）——
    /// 这样调用方不必为了拿 id 再查一次。
    pub async fn add(&self, subject: &str, content: &str, source: &str) -> Result<FactRow> {
        let row = sqlx::query_as::<_, FactRow>(
            "INSERT INTO facts (subject, content, source) VALUES (?, ?, ?)
             RETURNING id, subject, content, source, created_at",
        )
        .bind(subject)
        .bind(content)
        .bind(source)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// 关键词检索。`bm25()` 越小越相关，所以升序取前 `top_k`。
    ///
    /// 两级：先走 FTS5（trigram），没命中再退回 LIKE 子串扫描 ——
    /// 后者接住 FTS 切不出三元组的短查询词（中文两字词很常见）。
    ///
    /// 查询串是空的（全是标点）时返回空结果而不是报错 —— 检索门失败开放靠这个。
    pub async fn search(&self, query: &str, top_k: u32) -> Result<Vec<FactRow>> {
        if let Some(expr) = to_match_expr(query) {
            let hits = sqlx::query_as::<_, FactRow>(
                "SELECT f.id, f.subject, f.content, f.source, f.created_at
                 FROM facts_fts
                 JOIN facts f ON f.id = facts_fts.rowid
                 WHERE facts_fts MATCH ?
                 ORDER BY bm25(facts_fts)
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
        sqlx::query_as::<_, FactRow>(
            "SELECT id, subject, content, source, created_at
             FROM facts
             WHERE subject LIKE ? OR content LIKE ?
             ORDER BY id DESC
             LIMIT ?",
        )
        .bind(&pattern)
        .bind(&pattern)
        .bind(top_k as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(Into::into)
    }

    pub async fn recent(&self, limit: u32, offset: u32) -> Result<Vec<FactRow>> {
        sqlx::query_as::<_, FactRow>(
            "SELECT id, subject, content, source, created_at
             FROM facts ORDER BY id DESC LIMIT ? OFFSET ?",
        )
        .bind(limit as i64)
        .bind(offset as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// 全部事实按主题归拢（MEMORY.md 镜像用）—— 人看的时候，
    /// 同一主题的事实待在一起才像一份记忆。
    pub async fn all_by_subject(&self, limit: u32) -> Result<Vec<FactRow>> {
        sqlx::query_as::<_, FactRow>(
            "SELECT id, subject, content, source, created_at
             FROM facts ORDER BY subject, id LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// 忘记某个主题下的全部事实。触发器会同步清掉 FTS 索引。
    pub async fn forget_subject(&self, subject: &str) -> Result<u64> {
        let done = sqlx::query("DELETE FROM facts WHERE subject = ?")
            .bind(subject)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected())
    }

    /// 改一条事实的正文（manage_memory 用）。返回是否真的改了：
    /// id 不存在就 false，让调用方如实告诉模型。
    pub async fn update(&self, id: i64, content: &str) -> Result<bool> {
        let done = sqlx::query("UPDATE facts SET content = ? WHERE id = ?")
            .bind(content)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// 按编号删一条事实。触发器会同步清掉 FTS 索引。
    pub async fn delete(&self, id: i64) -> Result<bool> {
        let done = sqlx::query("DELETE FROM facts WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }
}
