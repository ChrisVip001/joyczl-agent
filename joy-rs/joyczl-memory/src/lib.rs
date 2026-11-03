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

pub mod compaction;
pub mod consolidation;
pub mod gate;
pub mod install;
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
        lines.push(format!("- **{}**: {}", fact.subject, fact.content));
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
        "_这是 Joy 记住的东西的人类可读镜像。事实来源是 `state.db`\
         （`facts` 与 `episodes` 两张表，FTS5 关键词可检索）；\
         本文件每轮之后重新生成。_"
            .to_string(),
        String::new(),
        format!("## Facts —— 语义记忆（{}）", facts.len()),
        String::new(),
    ];
    if facts.is_empty() {
        lines.push("_还没有_".to_string());
    } else {
        for fact in &facts {
            lines.push(format!("- **{}** — {}", fact.subject, fact.content));
        }
    }
    lines.push(String::new());
    lines.push(format!("## Episodes —— 情景记忆（{}）", episodes.len()));
    lines.push(String::new());
    if episodes.is_empty() {
        lines.push("_还没有_".to_string());
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
