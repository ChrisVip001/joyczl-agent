//! Semantic memory: long-term facts about the user, people, and projects.

use anyhow::Result;
use sqlx::sqlite::SqlitePool;

use crate::fts::{like_pattern, to_match_expr};

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct FactRow {
    pub id: i64,
    pub subject: String,
    pub content: String,
    /// `user` = said directly by the user; `consolidation` = distilled from conversation.
    pub source: String,
    /// What this fact is **about** (`user` / `feedback` / `project` / `reference` /
    /// `fact`). Anything unclassifiable falls to `fact`, never lost.
    pub kind: String,
    pub created_at: Option<String>,
}

/// The kinds of memory. `fact` is the fallback: unclassifiable or old rows live here.
pub const KINDS: [&str; 5] = ["fact", "user", "feedback", "project", "reference"];

/// Collapse foreign kinds into the known ones — unknown always becomes `fact`.
///
/// Collapsing happens at the **write entry**: the DB only holds these five; readers never re-check.
pub fn normalise_kind(kind: &str) -> &'static str {
    let trimmed = kind.trim().to_lowercase();
    KINDS
        .iter()
        .find(|known| **known == trimmed)
        .copied()
        .unwrap_or("fact")
}

/// The raw row for `all_with_embedding`: FactRow's five fields + the vector text
/// (`None` = the column is still NULL; the query already filters it; kept for type honesty).
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

    /// Write one fact, returning the full stored row (with id and created_at) —
    /// callers need not query again just for the id.
    /// Record a fact. Returns `(the fact, whether it is new)`.
    ///
    /// **Duplicates never enter the DB** (unique index from migration 0008: `(subject,
    /// lower(trim(content)))`): if it exists, return **the existing row** with `false`,
    /// letting the caller decide what to say.
    /// "Duplicate" is not an error — consolidation reruns and models repeat themselves; it's noise.
    pub async fn add(
        &self,
        subject: &str,
        content: &str,
        source: &str,
        kind: &str,
    ) -> Result<(FactRow, bool)> {
        let inserted = sqlx::query_as::<_, FactRow>(
            "INSERT INTO facts (subject, content, source, kind) VALUES (?, ?, ?, ?)
             ON CONFLICT DO NOTHING
             RETURNING id, subject, content, source, kind, created_at",
        )
        .bind(subject)
        .bind(content)
        .bind(source)
        .bind(normalise_kind(kind))
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = inserted {
            return Ok((row, true));
        }
        // Collided with an existing row: read it back and hand it over (the caller wants the fact).
        let existing = sqlx::query_as::<_, FactRow>(
            "SELECT id, subject, content, source, kind, created_at
             FROM facts
             WHERE subject = ? AND lower(trim(content)) = lower(trim(?))
             ORDER BY id LIMIT 1",
        )
        .bind(subject)
        .bind(content)
        .fetch_one(&self.pool)
        .await?;
        Ok((existing, false))
    }

    /// Store a fact's vector (JSON array text). If it can't be computed, skip — this column
    /// is a bonus, not part of the fact.
    pub async fn set_embedding(&self, id: i64, vector: &[f32]) -> Result<()> {
        let json = serde_json::to_string(vector)?;
        sqlx::query("UPDATE facts SET embedding = ? WHERE id = ?")
            .bind(json)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// All rows with vectors. At personal scale a full scan + cosine in Rust is plenty
    /// (see migration 0005's reasoning); not worth a vector extension.
    pub async fn all_with_embedding(&self) -> Result<Vec<(FactRow, Vec<f32>)>> {
        let rows: Vec<FactWithEmbedding> = sqlx::query_as(
            "SELECT id, subject, content, source, kind, created_at, embedding
             FROM facts WHERE embedding IS NOT NULL ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::new();
        for (id, subject, content, source, kind, created_at, embedding) in rows {
            // Unparsable vectors count as absent: they must not fail the whole retrieval.
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

    /// Facts without a computed vector yet (used by `joy memory reindex`).
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

    /// Keyword search. Lower `bm25()` = more relevant, so take the top `top_k` ascending.
    ///
    /// Two tiers: FTS5 (trigram) first, falling back to a LIKE substring scan on no hits —
    /// the latter catches short query terms FTS can't trigram (two-char CJK words are common).
    ///
    /// An empty query (all punctuation) returns empty results, not errors — the retrieval gate's fail-open depends on it.
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

    /// All facts grouped by subject (for the MEMORY.md mirror) — when a human reads,
    /// facts on the same subject belong together to look like a memory.
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

    /// Forget all facts under a subject. A trigger keeps the FTS index in sync.
    pub async fn forget_subject(&self, subject: &str) -> Result<u64> {
        let done = sqlx::query("DELETE FROM facts WHERE subject = ?")
            .bind(subject)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected())
    }

    /// Change a fact's content (manage_memory). Returns whether it really changed:
    /// a missing id is false, so the caller can tell the model honestly.
    pub async fn update(&self, id: i64, content: &str) -> Result<bool> {
        let done = sqlx::query("UPDATE facts SET content = ? WHERE id = ?")
            .bind(content)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Delete one fact by id. A trigger keeps the FTS index in sync.
    pub async fn delete(&self, id: i64) -> Result<bool> {
        let done = sqlx::query("DELETE FROM facts WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }
}
