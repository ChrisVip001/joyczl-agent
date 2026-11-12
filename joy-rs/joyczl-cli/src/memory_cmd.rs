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
            println!(
                "Unknown subcommand '{other}'. Options: reindex (default), backfill, rebuild."
            );
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
        println!("The ledger already holds {before} turns; not backfilling (it only runs once).");
        return Ok(());
    }
    println!("Backfilling the ledger from chat_log (historical tool output survives as summaries only, marked folded)…");
    let report = joyczl_state::ledger::backfill_from_chat(&pool).await?;
    println!(
        "Backfill done: {} sessions, {} turns, {} rows. Those turns can only be rebuilt down to the folded text — there was never a fuller record.",
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
        println!("Every fact already has a vector; nothing to do.");
        return Ok(());
    }
    println!("Adding vectors (embedding model: see JOY_EMBED_MODEL)…");
    let done = joyczl_memory::retrieval::reindex(&facts, &embedder).await?;
    println!("Added {done} vectors.");
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
        "Memory library: {} pages written, {} pages left alone (edited by hand), {} subjects have no facts left (pages kept)",
        report.written, report.skipped, report.orphaned
    );
    println!(
        "Catalog and change history are in {}",
        bundle.root().join("index.md").display()
    );
    Ok(())
}
