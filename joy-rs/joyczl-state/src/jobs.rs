//! 后台作业的台账（迁移 0009）。
//!
//! 这里只记**事实**（跑过什么、状态、退出码、输出在哪），不做调度、不做重跑。
//! 崩溃语义是五家参考实现的一致选择：启动对账把还挂着 `running` 的行标成
//! `interrupted`，输出文件保留 —— 重跑与否由人/模型看着记录决定。

use anyhow::Result;
use sqlx::{FromRow, SqlitePool};

/// 台账里的一行。
#[derive(Debug, Clone, FromRow)]
pub struct JobRow {
    pub id: String,
    pub owner: String,
    pub command: String,
    /// `running` / `completed` / `failed` / `killed` / `interrupted`。
    pub status: String,
    pub exit_code: Option<i32>,
    pub log_path: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

/// 作业台账。 便宜（只有一个连接池句柄）—— 调用方和测试常要两份。
#[derive(Clone)]
pub struct Jobs {
    pool: SqlitePool,
}

impl Jobs {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 作业起跑：记一行 `running`（started_at 由库的默认值填）。
    pub async fn record_started(
        &self,
        id: &str,
        owner: &str,
        command: &str,
        log_path: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO jobs (id, owner, command, status, log_path)
             VALUES (?, ?, ?, 'running', ?)",
        )
        .bind(id)
        .bind(owner)
        .bind(command)
        .bind(log_path)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 作业收尾。`status`：`completed` / `failed` / `killed`。
    pub async fn record_finished(
        &self,
        id: &str,
        status: &str,
        exit_code: Option<i32>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE jobs SET status = ?, exit_code = ?, finished_at = datetime('now') WHERE id = ?",
        )
        .bind(status)
        .bind(exit_code)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 启动对账：把还挂着 `running` 的行（上次崩溃的遗物）标成 `interrupted`，
    /// 返回它们 —— 调用方负责逐条打出「发生了什么」的日志。
    pub async fn reconcile_interrupted(&self) -> Result<Vec<JobRow>> {
        let rows = sqlx::query_as::<_, JobRow>(
            "SELECT id, owner, command, status, exit_code, log_path, started_at, finished_at
             FROM jobs WHERE status = 'running'",
        )
        .fetch_all(&self.pool)
        .await?;
        sqlx::query(
            "UPDATE jobs SET status = 'interrupted', finished_at = datetime('now')
             WHERE status = 'running'",
        )
        .execute(&self.pool)
        .await?;
        Ok(rows)
    }

    /// 某个 owner 的历史作业（含崩溃遗留），新的在前。
    pub async fn recent(&self, owner: &str, limit: u32) -> Result<Vec<JobRow>> {
        Ok(sqlx::query_as::<_, JobRow>(
            "SELECT id, owner, command, status, exit_code, log_path, started_at, finished_at
             FROM jobs WHERE owner = ?
             ORDER BY started_at DESC LIMIT ?",
        )
        .bind(owner)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 按 id 查一行（owner 围栏由调用方核对）。
    pub async fn get(&self, id: &str) -> Result<Option<JobRow>> {
        Ok(sqlx::query_as::<_, JobRow>(
            "SELECT id, owner, command, status, exit_code, log_path, started_at, finished_at
             FROM jobs WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }
}
