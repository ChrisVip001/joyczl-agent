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
        Some(other) => {
            println!("不认识的子命令 '{other}'。可用：reindex（默认）。");
            Ok(())
        }
    }
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
