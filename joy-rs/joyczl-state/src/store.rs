//! The memory-backend contracts.
//!
//! Joy stays neutral about "where facts live": today only SQLite, tomorrow pgvector or a
//! hosted memory API. That neutrality has one premise: **every backend performs the same
//! set of actions**. Without a contract, a second backend missing two methods turns a loud
//! error into a quiet lie via call-site defensive fallbacks — the agent tells the user
//! "no such memory" while the memory sits in the store.
//!
//! So: implement these two traits, then run the same acceptance suite in
//! [`crate::conformance`] — any half-built backend shows up in CI, not in front of users.
//!
//! Contract essentials (implementers, read this):
//!   * **A miss never errors** — return an empty list / `false`; a thrower takes the turn down.
//!   * An empty `search` must mean "no hit", never "the search didn't run".
//!   * Local storage is naturally synchronous (write = indexed); if a hosted backend is
//!     eventually consistent, handle readiness in your own implementation — no `settle` in
//!     the contract: upstream shouldn't pay interface complexity for one backend's trait.

use anyhow::Result;

use crate::{EpisodeRow, Episodes, FactRow, Facts};

/// Semantic memory: long-term facts about the user, people, projects, preferences.
///
/// The `async_fn_in_trait` tradeoff is the same as mcp's `Rpc` trait: the interface is for
/// "in this repo, passed as `impl Trait`" implementations, without paying Send-boxing
/// complexity for dyn compat. A real cross-crate remote backend can upgrade to BoxFuture later.
#[allow(async_fn_in_trait)]
pub trait SemanticStore {
    /// Store a fact. `source` records who said it ("user" / "consolidation") — provenance matters.
    async fn add(&self, subject: &str, content: &str, source: &str, kind: &str) -> Result<FactRow>;
    /// Search by keyword, relevance desc. No hit = empty vec, never an error.
    async fn search(&self, query: &str, top_k: u32) -> Result<Vec<FactRow>>;
    /// Most recent facts (id desc), used by dashboard and list_memory.
    async fn recent(&self, limit: u32, offset: u32) -> Result<Vec<FactRow>>;
    /// Change a fact's content. Missing id → `false`, no error.
    async fn update(&self, id: i64, content: &str) -> Result<bool>;
    /// Delete a fact. Missing id → `false`, no error.
    async fn delete(&self, id: i64) -> Result<bool>;
    /// Forget all facts under a subject; returns how many were actually deleted.
    async fn forget_subject(&self, subject: &str) -> Result<u64>;
}

/// Episodic memory: things that happened, with dates.
#[allow(async_fn_in_trait)]
pub trait EpisodicStore {
    async fn add(&self, happened_at: &str, summary: &str) -> Result<i64>;
    async fn search(&self, query: &str, top_k: u32) -> Result<Vec<EpisodeRow>>;
    async fn recent(&self, limit: u32) -> Result<Vec<EpisodeRow>>;
    async fn delete(&self, id: i64) -> Result<bool>;
}

impl SemanticStore for Facts {
    async fn add(&self, subject: &str, content: &str, source: &str, kind: &str) -> Result<FactRow> {
        // The storage abstraction doesn't need "is it new"; hand over the row (dedup still happens).
        Ok(Facts::add(self, subject, content, source, kind).await?.0)
    }
    async fn search(&self, query: &str, top_k: u32) -> Result<Vec<FactRow>> {
        Facts::search(self, query, top_k).await
    }
    async fn recent(&self, limit: u32, offset: u32) -> Result<Vec<FactRow>> {
        Facts::recent(self, limit, offset).await
    }
    async fn update(&self, id: i64, content: &str) -> Result<bool> {
        Facts::update(self, id, content).await
    }
    async fn delete(&self, id: i64) -> Result<bool> {
        Facts::delete(self, id).await
    }
    async fn forget_subject(&self, subject: &str) -> Result<u64> {
        Facts::forget_subject(self, subject).await
    }
}

impl EpisodicStore for Episodes {
    async fn add(&self, happened_at: &str, summary: &str) -> Result<i64> {
        Episodes::add(self, happened_at, summary).await
    }
    async fn search(&self, query: &str, top_k: u32) -> Result<Vec<EpisodeRow>> {
        Episodes::search(self, query, top_k).await
    }
    async fn recent(&self, limit: u32) -> Result<Vec<EpisodeRow>> {
        Episodes::recent(self, limit).await
    }
    async fn delete(&self, id: i64) -> Result<bool> {
        Episodes::delete(self, id).await
    }
}
