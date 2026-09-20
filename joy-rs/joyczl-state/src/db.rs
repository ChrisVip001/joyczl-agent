//! 打开 state.db 并跑到最新 schema。

use std::path::Path;

use anyhow::{Context, Result};
use sqlx::migrate::MigrateDatabase;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};

/// 迁移脚本在编译期被读进来，所以二进制自带 schema，不依赖运行时找文件。
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// 打开（必要时创建）`path` 处的 state.db，并跑完所有迁移。
pub async fn open(path: &Path) -> Result<SqlitePool> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建 state 目录失败：{}", parent.display()))?;
        }
    }

    // sqlite:// 后面必须是绝对路径，否则相对路径会被当成相对于当前工作目录
    // 解析 —— 而 app-server 的工作目录不该决定数据库在哪。
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

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .with_context(|| format!("连接失败：{url}"))?;

    // 3 秒等锁：app-server 单进程内多个请求并发写时，别直接报 "database is locked"。
    sqlx::query("PRAGMA busy_timeout = 3000")
        .execute(&pool)
        .await?;
    // WAL：读不阻塞写，dashboard 一边轮询一边聊天不会被卡住。
    sqlx::query("PRAGMA journal_mode = WAL")
        .execute(&pool)
        .await?;

    MIGRATOR.run(&pool).await.context("跑迁移失败")?;
    Ok(pool)
}
