//! `joy memory` — local maintenance commands for memory.
//!
//! `joy memory reindex` backfills vectors for facts that don't have one yet
//! (the ticket into the `JOY_EMBEDDINGS=1` leg). Memory written before the flag
//! was enabled, and facts whose embedding service happened to be down at write
//! time, are caught up here — skipping it loses nothing, they just stay on the
//! keyword leg for now.

use anyhow::Result;
use joyczl_provider::embed::Embedder;
use joyczl_state::Facts;

pub async fn run(settings: &joyczl_config::Settings, args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        None | Some("reindex") => reindex(settings).await,
        Some("backfill") => backfill(settings).await,
        Some("rebuild") => rebuild(settings).await,
        Some(other) => {
            println!("不认识的子命令 '{other}'。可用：reindex（默认）、backfill、rebuild。");
            Ok(())
        }
    }
}

/// Turn what `chat_log` already holds into ledger rows, marked `folded`.
///
/// Only useful once (after migration 0020) and honest about its ceiling: the view it
/// reads was already truncated to 200 characters per tool output, so those turns cannot
/// be rebuilt exactly — the `folded` mark says so on every row.
async fn backfill(settings: &joyczl_config::Settings) -> Result<()> {
    let pool = joyczl_state::open(&settings.home.join("state.db")).await?;
    let before = joyczl_state::Ledger::new(pool.clone()).count().await?;
    if before > 0 {
        println!("账本里已经有 {before} 轮，不再回填（回填只跑一次）。");
        return Ok(());
    }
    println!("从 chat_log 回填账本（历史工具输出只剩摘要，会标成 folded）……");
    let report = joyczl_state::ledger::backfill_from_chat(&pool).await?;
    println!(
        "回填完成：{} 个会话、{} 轮、{} 行。这些轮的重建只能到折叠文本为止 —— 当时就没有更完整的记录。",
        report.sessions, report.turns, report.rows
    );
    Ok(())
}

async fn reindex(settings: &joyczl_config::Settings) -> Result<()> {
    let embedder = Embedder::from_settings(settings).map_err(|e| anyhow::anyhow!("{e}"))?;
    let pool = joyczl_state::open(&settings.home.join("state.db")).await?;
    let facts = Facts::new(pool);

    let pending = facts.missing_embedding(1).await?.len();
    if pending == 0 {
        println!("所有事实都已有向量，不需要补。");
        return Ok(());
    }
    println!("开始补向量（embedding 模型：见 JOY_EMBED_MODEL）……");
    let done = joyczl_memory::retrieval::reindex(&facts, &embedder).await?;
    println!("补了 {done} 条向量。");
    Ok(())
}

/// Regenerate the plaintext library from the facts (`<home>/memory/`).
///
/// Not a migration: a maintenance command for the day a page was deleted, a fact was
/// edited behind the file's back, or someone wants to see the library rebuilt from the
/// index. Pages a human edited are left alone (and counted), because the file wins.
async fn rebuild(settings: &joyczl_config::Settings) -> Result<()> {
    let pool = joyczl_state::open(&settings.home.join("state.db")).await?;
    let facts = Facts::new(pool);
    let bundle = joyczl_memory::bundle::Bundle::new(&settings.home);
    let report = joyczl_memory::mirror::mirror_facts(&facts, &bundle).await?;
    println!(
        "记忆库：{} 页写入，{} 页被手动改过没动，{} 个主题已经没事实了（页面保留）",
        report.written, report.skipped, report.orphaned
    );
    println!(
        "目录与变更历史在 {}",
        bundle.root().join("index.md").display()
    );
    Ok(())
}
