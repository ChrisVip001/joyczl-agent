//! 打开 state.db 并跑到最新 schema。

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::migrate::MigrateDatabase;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

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

    // 并发参数必须挂在**连接选项**上，不能事后 `PRAGMA`：
    // `PRAGMA busy_timeout` 是 per-connection 的，而 `execute(&pool)` 只作用于
    // 恰好接到那条查询的连接 —— 池里另外 7 个连接仍是默认值。实测踩过：设了
    // 3000，读回来是 sqlx 的默认 5000，而注释一直写着 3000。
    //
    // 10 秒等锁：**多进程**同时写时（REPL、dashboard 的 app-server 子进程、
    // `joy mcp serve`、`joy schedule` 可以同时开着同一个 state.db），排队比
    // 直接报 "database is locked" 好。SQLite 官方建议 10s。
    let options = SqliteConnectOptions::new()
        .filename(&absolute)
        .create_if_missing(false)
        .busy_timeout(Duration::from_secs(10))
        // WAL：读不阻塞写，dashboard 一边轮询一边聊天不会被卡住。
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await
        .with_context(|| format!("连接失败：{url}"))?;

    MIGRATOR.run(&pool).await.context("跑迁移失败")?;
    Ok(pool)
}
