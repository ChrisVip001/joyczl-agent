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

/// The provenance/freshness sidecar of a fact (migration 0020).
///
/// Kept **outside** `FactRow` on purpose: `FactRow` is the memory itself and every
/// reader selects its six columns; bolting four more onto it would rewrite every query
/// for information only the writer and the memory center need.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct FactMeta {
    pub id: i64,
    /// Where this fact lives in the plaintext bundle (`<home>/memory/…`), once mirrored.
    pub page_path: Option<String>,
    /// The page's digest at mirror time; a mismatch means a human edited the file.
    pub digest: Option<String>,
    /// Last time this fact was written or re-observed. `None` = never (old rows).
    pub last_confirmed_at: Option<String>,
    /// How many times it has been observed. A fact seen once is a claim; seen five
    /// times across months is a belief.
    pub confirm_count: i64,
}

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
        edits::add(self, subject, content, source, kind)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Memory suppressed by a prior human edit; explicitly remember it to restore it"
                )
            })
    }

    /// Background extraction cannot undo a human deletion or correction.
    pub async fn add_consolidated(
        &self,
        subject: &str,
        content: &str,
        kind: &str,
    ) -> Result<Option<(FactRow, bool)>> {
        edits::add(self, subject, content, "consolidation", kind).await
    }

    /// Persist extraction context atomically with the fact. Replaying the same context
    /// does not count as another observation; references are derivation inputs, not proof.
    pub async fn add_consolidated_from(
        &self,
        subject: &str,
        content: &str,
        kind: &str,
        source_refs: &[String],
    ) -> Result<Option<(FactRow, bool)>> {
        edits::add_from(self, subject, content, "consolidation", kind, source_refs).await
    }

    /// All derivation contexts for a fact, in insertion order. Deleted facts have none.
    pub async fn observations(&self, id: i64) -> Result<Vec<(String, String, String)>> {
        Ok(sqlx::query_as("SELECT context_digest,source_refs_json,observed_at FROM memory_observations WHERE fact_id=? ORDER BY rowid")
            .bind(id).fetch_all(&self.pool).await?)
    }

    /// Mark a fact as still true: bump its count and stamp the moment.
    ///
    /// This is the only freshness signal the store has. Without it a six-month-old
    /// claim and yesterday's look identical to both the retriever and the human.
    pub async fn confirm(&self, id: i64) -> Result<()> {
        sqlx::query(
            "UPDATE facts
             SET confirm_count = confirm_count + 1, last_confirmed_at = datetime('now')
             WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The provenance/freshness sidecar of one fact, or `None` if the id is gone.
    pub async fn meta(&self, id: i64) -> Result<Option<FactMeta>> {
        let row = sqlx::query_as::<_, FactMeta>(
            "SELECT id, page_path, digest, last_confirmed_at, confirm_count
             FROM facts WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Record that a fact is mirrored at `page_path` with `digest` — the index side of
    /// "the file is authoritative, the row is the pointer".
    pub async fn set_page(&self, id: i64, page_path: &str, digest: &str) -> Result<()> {
        sqlx::query("UPDATE facts SET page_path = ?, digest = ? WHERE id = ?")
            .bind(page_path)
            .bind(digest)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Every fact of one subject, oldest first — the mirror's page content.
    pub async fn for_subject(&self, subject: &str) -> Result<Vec<FactRow>> {
        let rows = sqlx::query_as::<_, FactRow>(
            "SELECT id, subject, content, source, kind, created_at
             FROM durable_facts WHERE subject = ? ORDER BY id",
        )
        .bind(subject)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// How many facts exist in total — the worker's "did anything change?" probe.
    pub async fn count(&self) -> Result<i64> {
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM facts")
            .fetch_one(&self.pool)
            .await?;
        Ok(total)
    }

    /// Every subject that still has facts, with its count — the mirror's walk order.
    /// Ordered by subject so a re-run produces the same file order (small git diffs).
    pub async fn subjects(&self) -> Result<Vec<(String, i64)>> {
        let rows = sqlx::query_as::<_, (String, i64)>(
            "SELECT subject, COUNT(*) FROM durable_facts GROUP BY subject ORDER BY subject",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Facts nobody has confirmed for `days` days, newest-first ignored — what the
    /// memory center marks as "getting stale" and what the retriever can hedge on.
    pub async fn stale(&self, days: u32, limit: u32) -> Result<Vec<FactRow>> {
        sqlx::query_as::<_, FactRow>(
            "SELECT id, subject, content, source, kind, created_at
             FROM active_facts
             WHERE last_confirmed_at IS NULL
                OR last_confirmed_at < datetime('now', ?)
             ORDER BY COALESCE(last_confirmed_at, created_at) LIMIT ?",
        )
        .bind(format!("-{days} days"))
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(Into::into)
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
             FROM active_facts WHERE embedding IS NOT NULL ORDER BY id",
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
             FROM active_facts WHERE embedding IS NULL ORDER BY id LIMIT ?",
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
                 JOIN active_facts f ON f.id = facts_fts.rowid
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
             FROM active_facts
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
             FROM active_facts ORDER BY id DESC LIMIT ? OFFSET ?",
        )
        .bind(limit as i64)
        .bind(offset as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// Bounded personal context for tool-free chat; consolidated inferences and bounded
    /// claims need the full retrieval path rather than becoming standing preferences.
    pub async fn personal_preferences(&self) -> Result<Vec<FactRow>> {
        Ok(sqlx::query_as("SELECT id,subject,content,source,kind,created_at FROM durable_facts WHERE source='user' AND kind IN ('user','feedback') ORDER BY last_confirmed_at DESC,id DESC LIMIT 6")
            .fetch_all(&self.pool).await?)
    }

    /// All facts grouped by subject (for the MEMORY.md mirror) — when a human reads,
    /// facts on the same subject belong together to look like a memory.
    pub async fn all_by_subject(&self, limit: u32) -> Result<Vec<FactRow>> {
        sqlx::query_as::<_, FactRow>(
            "SELECT id, subject, content, source, kind, created_at
             FROM durable_facts ORDER BY subject, id LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// Forget all facts under a subject. A trigger keeps the FTS index in sync.
    pub async fn forget_subject(&self, subject: &str) -> Result<u64> {
        edits::forget_subject(self, subject).await
    }

    /// Change a fact's content (manage_memory). Returns whether it really changed:
    /// a missing id is false, so the caller can tell the model honestly.
    pub async fn update(&self, id: i64, content: &str) -> Result<bool> {
        edits::update(self, id, content).await
    }

    /// Delete one fact by id. A trigger keeps the FTS index in sync.
    pub async fn delete(&self, id: i64) -> Result<bool> {
        edits::delete(self, id).await
    }
}

#[path = "fact_edits.rs"]
mod edits;

#[path = "memory_validity.rs"]
mod validity;

#[path = "transient_memory.rs"]
mod transient;
pub use transient::TransientObservation;

impl Facts {
    /// Persist a source-bound observation and expiry in the same transaction.
    pub async fn add_transient_from(
        &self,
        subject: &str,
        content: &str,
        kind: &str,
        source_refs: &[String],
        observation: &TransientObservation,
    ) -> Result<Option<(FactRow, bool)>> {
        edits::add_observed(
            self,
            subject,
            content,
            "consolidation",
            kind,
            edits::Observation {
                refs: source_refs,
                transient: Some(observation),
            },
        )
        .await
    }

    pub async fn observations_detailed(
        &self,
        id: i64,
    ) -> Result<Vec<(String, String, String, Option<String>)>> {
        Ok(sqlx::query_as("SELECT o.context_digest,o.source_refs_json,o.observed_at,a.assessment_json FROM memory_observations o LEFT JOIN memory_transient_assessments a ON a.fact_id=o.fact_id AND a.context_digest=o.context_digest WHERE o.fact_id=? ORDER BY o.rowid")
            .bind(id).fetch_all(&self.pool).await?)
    }

    pub async fn validity_origin(&self, id: i64) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT origin FROM memory_validity WHERE fact_id=?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }
}
