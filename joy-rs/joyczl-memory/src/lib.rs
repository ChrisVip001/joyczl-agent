//! `joyczl-memory` —— 记忆的智能体们。
//!
//! 存储在 joyczl-state；这一层管的是**决策**：
//!
//!   * `gate`           这条消息需不需要查记忆？（hero moment #1）
//!   * `consolidation`  积累了几轮对话之后，从里面提炼值得长期记住的事实。
//!   * `skills`         过程记忆：SKILL.md 只在相关时载入（渐进披露）。
//!
//! 前两者都刻意**失败开放**：gate 出错就照常检索（过时的记忆也好过丢失的），
//! consolidation 出错就让日志留着不提炼（下次再来，原始记录绝不丢）。

pub mod consolidation;
pub mod gate;
pub mod skills;

#[cfg(test)]
#[path = "memory_tests.rs"]
mod memory_tests;

use std::path::Path;

use joyczl_state::{Episodes, Facts};

/// 拼进 system prompt 的记忆上下文。空字符串 = 这次没检索到东西，
/// 调用方就不要往 prompt 里塞「相关记忆」的标题了。
pub async fn retrieve_context(
    facts: &Facts,
    episodes: &Episodes,
    query: &str,
    top_k: u32,
) -> anyhow::Result<String> {
    let found = facts.search(query, top_k).await?;
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

/// 把记忆镜像成一份人类可读的 `<home>/MEMORY.md` ——
/// 「你的记忆就是一个能打开的文件」这句话要成立。state.db 仍是可查询的
/// 事实来源；这个文件是**生成的视图**，每轮之后刷新。
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
