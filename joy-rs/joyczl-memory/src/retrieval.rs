//! Hybrid retrieval: the keyword leg plus the vector leg, fused with RRF.
//!
//! Keywords alone: asking "that launch plan from before" never finds "ship on Oct 15" —
//! not one literal word overlaps. Vectors alone blur proper names (`JOY_HOME`,
//! `state.db`). Each leg ranks on its own, then fuse by rank — they complement.
//!
//! **Fuse by rank, not score** (RRF: `Σ 1/(k + rank)`, k=60): bm25 and cosine
//! similarity have entirely different scales; forcing one score is making data up.
//! Rank is the one thing both have that compares. k=60 is the paper default.
//!
//! Fail-open, like the rest of this crate: when the embedding service is down, use
//! the keyword leg only — one network failure must never become "remembering nothing".

use anyhow::Result;
use joyczl_provider::embed::Embedder;
use joyczl_state::{FactRow, Facts};

/// Similarity threshold for the vector leg. Below it counts as unrelated — cosine
/// similarity under 0.3 usually means "same language", not "same topic".
const MIN_SIMILARITY: f32 = 0.30;

/// RRF's rank-smoothing constant (paper default).
const RRF_K: f32 = 60.0;

/// Cosine similarity. Mismatched lengths or zero vectors return 0 (unrelated, not an error).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// RRF fusion of the two result sets. On rank ties, the earlier (keyword leg) comes
/// first — an exact match is always more believable than "probably relevant".
pub fn rrf_fuse(keyword: &[FactRow], semantic: &[FactRow]) -> Vec<FactRow> {
    use std::collections::HashMap;

    let mut scores: HashMap<i64, f32> = HashMap::new();
    let mut rows: HashMap<i64, FactRow> = HashMap::new();
    for (rank, row) in keyword.iter().enumerate() {
        *scores.entry(row.id).or_insert(0.0) += 1.0 / (RRF_K + rank as f32 + 1.0);
        rows.entry(row.id).or_insert_with(|| row.clone());
    }
    for (rank, row) in semantic.iter().enumerate() {
        *scores.entry(row.id).or_insert(0.0) += 1.0 / (RRF_K + rank as f32 + 1.0);
        rows.entry(row.id).or_insert_with(|| row.clone());
    }

    let mut ranked: Vec<(i64, f32)> = scores.into_iter().collect();
    // Score descending; ties by id ascending, keeping results stable (same query, same order).
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    ranked
        .into_iter()
        .filter_map(|(id, _)| rows.remove(&id))
        .collect()
}

/// The vector leg: compute the query vector and compare against every vectorized row.
///
/// On error return empty — the caller (`search_hybrid`) falls back to pure keywords.
async fn semantic_hits(
    facts: &Facts,
    embedder: &Embedder,
    query: &str,
    top_k: usize,
) -> Result<Vec<FactRow>> {
    let query_vector = embedder.embed(query).await?;
    let mut scored: Vec<(f32, FactRow)> = facts
        .all_with_embedding()
        .await?
        .into_iter()
        .filter_map(|(row, vector)| {
            let score = cosine(&query_vector, &vector);
            (score >= MIN_SIMILARITY).then_some((score, row))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.id.cmp(&b.1.id))
    });
    Ok(scored.into_iter().take(top_k).map(|(_, row)| row).collect())
}

/// Hybrid retrieval. No embedder given (flag off) means pure keywords — exactly as before the upgrade.
pub async fn search_hybrid(
    facts: &Facts,
    embedder: Option<&Embedder>,
    query: &str,
    top_k: u32,
) -> Vec<FactRow> {
    let keyword = facts.search(query, top_k).await.unwrap_or_default();
    let Some(embedder) = embedder else {
        return keyword;
    };
    let semantic = match semantic_hits(facts, embedder, query, top_k as usize * 2).await {
        Ok(hits) => hits,
        Err(e) => {
            // One broken leg isn't fatal: the keyword leg still stands.
            eprintln!("(joy) 向量检索不可用，这次只用关键词：{e}");
            return keyword;
        }
    };
    if semantic.is_empty() {
        return keyword;
    }
    rrf_fuse(&keyword, &semantic)
        .into_iter()
        .take(top_k as usize)
        .collect()
}

/// The body of `joy memory reindex`: backfills vectors for facts that lack one.
/// Returns how many were backfilled.
pub async fn reindex(facts: &Facts, embedder: &Embedder) -> Result<usize> {
    let mut done = 0usize;
    loop {
        let batch = facts.missing_embedding(50).await?;
        if batch.is_empty() {
            return Ok(done);
        }
        for row in batch {
            // Embed subject + content together: the subject is often what retrieval asks for.
            let text = format!("{}: {}", row.subject, row.content);
            match embedder.embed(&text).await {
                Ok(vector) => {
                    facts.set_embedding(row.id, &vector).await?;
                    done += 1;
                }
                Err(e) => {
                    anyhow::bail!("第 {} 条算不出来（{e}）—— 已补 {done} 条", row.id);
                }
            }
        }
    }
}
