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
    /// 这条事实**关于什么**（`user` / `feedback` / `project` / `reference` /
    /// `fact`）。分类不出来的落 `fact`，永远不丢。
    pub kind: String,
    pub created_at: Option<String>,
}

/// 记忆的类别。`fact` 是兜底：分类不出来、或者老行，都在这里。
pub const KINDS: [&str; 5] = ["fact", "user", "feedback", "project", "reference"];

/// 把外来的类别收敛到已知的那几个 —— 未知的一律当 `fact`。
///
/// 收敛放在**写入口**：库里只会有这五种，读的地方不必各自容错。
pub fn normalise_kind(kind: &str) -> &'static str {
    let trimmed = kind.trim().to_lowercase();
    KINDS
        .iter()
        .find(|known| **known == trimmed)
        .copied()
        .unwrap_or("fact")
}

/// `all_with_embedding` 的原始行：FactRow 的五个字段 + 向量文本
/// （`None` = 这一列还是 NULL，虽然查询已经过滤过，留着类型诚实）。
type FactWithEmbedding = (
    i64,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
);

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
    pub async fn add(
        &self,
        subject: &str,
        content: &str,
        source: &str,
        kind: &str,
    ) -> Result<FactRow> {
        let row = sqlx::query_as::<_, FactRow>(
            "INSERT INTO facts (subject, content, source, kind) VALUES (?, ?, ?, ?)
             RETURNING id, subject, content, source, kind, created_at",
        )
        .bind(subject)
        .bind(content)
        .bind(source)
        .bind(normalise_kind(kind))
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// 存一条事实的向量（JSON 数组文本）。算不出来就不算 —— 这一列是
    /// 加分项，不是事实的一部分。
    pub async fn set_embedding(&self, id: i64, vector: &[f32]) -> Result<()> {
        let json = serde_json::to_string(vector)?;
        sqlx::query("UPDATE facts SET embedding = ? WHERE id = ?")
            .bind(json)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 所有带向量的行。个人规模下全表扫描 + Rust 侧算余弦完全够用
    /// （见 0005 迁移里的理由），不值得为它引一个向量扩展。
    pub async fn all_with_embedding(&self) -> Result<Vec<(FactRow, Vec<f32>)>> {
        let rows: Vec<FactWithEmbedding> = sqlx::query_as(
            "SELECT id, subject, content, source, kind, created_at, embedding
             FROM facts WHERE embedding IS NOT NULL ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::new();
        for (id, subject, content, source, kind, created_at, embedding) in rows {
            // 解析不了的向量当没有：它不该让整次检索失败。
            let Some(vector) = embedding.and_then(|raw| serde_json::from_str(&raw).ok()) else {
                continue;
            };
            out.push((
                FactRow {
                    id,
                    subject,
                    content,
                    source,
                    kind,
                    created_at,
                },
                vector,
            ));
        }
        Ok(out)
    }

    /// 还没算过向量的事实（`joy memory reindex` 用）。
    pub async fn missing_embedding(&self, limit: u32) -> Result<Vec<FactRow>> {
        sqlx::query_as::<_, FactRow>(
            "SELECT id, subject, content, source, kind, created_at
             FROM facts WHERE embedding IS NULL ORDER BY id LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(Into::into)
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
                "SELECT f.id, f.subject, f.content, f.source, f.kind, f.created_at
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
            "SELECT id, subject, content, source, kind, created_at
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
            "SELECT id, subject, content, source, kind, created_at
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
            "SELECT id, subject, content, source, kind, created_at
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
