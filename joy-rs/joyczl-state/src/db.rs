//! Open state.db and migrate it to the latest schema.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::migrate::MigrateDatabase;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

/// Migration scripts are read in at compile time: the binary carries its schema, no runtime file lookup.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Open (creating if needed) the state.db at `path`, and run all migrations.
pub async fn open(path: &Path) -> Result<SqlitePool> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建 state 目录失败：{}", parent.display()))?;
        }
    }

    // After sqlite:// must be an absolute path, or the relative path resolves against the
    // current working directory — and the app-server's cwd must not decide where the DB is.
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let url = format!("sqlite://{}", absolute.display());

    if !sqlx::Sqlite::database_exists(&url).await.unwrap_or(false) {
        sqlx::Sqlite::create_database(&url)
            .await
            .with_context(|| format!("创建数据库失败：{url}"))?;
    }

    // Concurrency knobs must sit on the **connect options**, not a later `PRAGMA`:
    // `PRAGMA busy_timeout` is per-connection, and `execute(&pool)` only hits whichever
    // connection happens to take that query — the other 7 in the pool keep the default.
    // Bitten in practice: set 3000, read back sqlx's default 5000, while the comment said 3000.
    //
    // 10s lock wait: with **multiple processes** writing at once (REPL, the dashboard's
    // app-server subprocess, `joy mcp serve`, `joy schedule` can hold the same state.db),
    // queuing beats "database is locked" errors. SQLite's own recommendation is 10s.
    let options = SqliteConnectOptions::new()
        .filename(&absolute)
        .create_if_missing(false)
        .busy_timeout(Duration::from_secs(10))
        // WAL: reads never block writes; the dashboard can poll while a chat is running.
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await
        .with_context(|| format!("连接失败：{url}"))?;

    MIGRATOR.run(&pool).await.context("跑迁移失败")?;
    Ok(pool)
}
