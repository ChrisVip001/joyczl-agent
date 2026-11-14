//! `joyczl-memory` — the memory agents.
//!
//! Storage lives in joyczl-state; this layer owns the **decisions**:
//!
//!   * `gate`           does this message need a memory lookup? (hero moment #1)
//!   * `consolidation`  after several turns, distills facts worth keeping long-term.
//!   * `skills`         procedural memory: SKILL.md loaded only when relevant (progressive disclosure).
//!
//! Both of the first two deliberately **fail open**: if gate errors, retrieve as usual
//! (stale beats lost); if consolidation errors, logs stay undistilled (raw records stay).

pub mod bundle;
pub mod compaction;
pub mod consolidation;
pub mod frontmatter;
pub mod gate;
pub mod install;
pub mod mirror;
pub mod retrieval;
pub mod skills;

#[cfg(test)]
#[path = "memory_tests.rs"]
mod memory_tests;

#[cfg(test)]
#[path = "compaction_tests.rs"]
mod compaction_tests;

#[cfg(test)]
#[path = "retrieval_tests.rs"]
mod retrieval_tests;

#[cfg(test)]
#[path = "install_tests.rs"]
mod install_tests;

use std::path::Path;

use joyczl_state::{Episodes, Facts};

/// The memory context spliced into the system prompt. Empty string = nothing found
/// this time; the caller then skips the "relevant memories" header.
pub async fn retrieve_context(
    facts: &Facts,
    episodes: &Episodes,
    query: &str,
    top_k: u32,
    embedder: Option<&joyczl_provider::embed::Embedder>,
) -> anyhow::Result<String> {
    // The facts line uses hybrid retrieval (pure keywords when the vector flag is off,
    // exactly as before); episodes stay keyword — they carry dates, vectors add little.
    let found = retrieval::search_hybrid(facts, embedder, query, top_k).await;
    let episodes = episodes.search(query, 3).await?;

    let mut lines = Vec::new();
    for fact in &found {
        let (_, until) = facts.validity(fact.id).await?;
        let temporal = until
            .map(|time| {
                format!(" (temporary claim, valid until {time}; not proof of current mood)")
            })
            .unwrap_or_default();
        lines.push(format!(
            "- **{}**: {}{}",
            fact.subject, fact.content, temporal
        ));
    }
    if !episodes.is_empty() {
        lines.push(
            "Historical episode summaries (summary date, not proof of current state):".into(),
        );
    }
    for episode in &episodes {
        lines.push(format!("- ({}) {}", episode.happened_at, episode.summary));
    }
    Ok(lines.join("\n"))
}

/// Mirrors memory into a human-readable `<home>/MEMORY.md` — "your memory is a file
/// you can open" must hold. state.db remains the queryable source of truth; this file
/// is a **generated view**, refreshed after each turn.
pub async fn export_markdown(
    facts: &Facts,
    episodes: &Episodes,
    home: &Path,
) -> anyhow::Result<()> {
    let facts = facts.all_by_subject(1_000).await?;
    let episodes = episodes.recent(1_000).await?;

    let mut lines = vec![
        "# Joy memory".to_string(),
        String::new(),
        "_A human-readable mirror of what Joy remembers. The facts live in `state.db`\
         (`facts` and `episodes`, keyword-searchable through FTS5);\
         this file is regenerated after every turn._"
            .to_string(),
        String::new(),
        format!("## Facts — semantic memory ({})", facts.len()),
        String::new(),
    ];
    if facts.is_empty() {
        lines.push("_nothing yet_".to_string());
    } else {
        for fact in &facts {
            lines.push(format!("- **{}** — {}", fact.subject, fact.content));
        }
    }
    lines.push(String::new());
    lines.push(format!(
        "## Episodes — episodic memory ({})",
        episodes.len()
    ));
    lines.push(String::new());
    if episodes.is_empty() {
        lines.push("_nothing yet_".to_string());
    } else {
        for episode in &episodes {
            lines.push(format!(
                "- **{}** — {}",
                episode.happened_at, episode.summary
            ));
        }
    }
    std::fs::write(home.join("MEMORY.md"), lines.join("\n") + "\n")?;
    Ok(())
}

#[cfg(test)]
mod consolidation_recovery_tests;
