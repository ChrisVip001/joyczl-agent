//! `joy memory` —— 记忆的本地维护命令。
//!
//! `joy memory reindex` 给还没有向量的事实补上向量（`JOY_EMBEDDINGS=1`
//! 那条腿的入场券）。开关打开之前的记忆、以及写入时 embedding 服务恰好
//! 不可用的事实，都靠它补齐 —— 不补也不会丢，只是暂时只有关键词那条腿。

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
