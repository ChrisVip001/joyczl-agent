//! The long-task ledger (migration 0014).
//!
//! A goal used to be a string in process memory: gone on restart, unsuspendable by nature.
//! This store makes it an entity — identity, name, status, round account — that outlives the
//! process. The goal loop keeps driving turns; the row keeps the truth.
//!
//! One active task per session, enforced by a partial unique index. Terminal states
//! (`done` / `failed` / `cancelled`) close the row and free the session for the next task.

use anyhow::Result;
use sqlx::{FromRow, SqlitePool};

/// One task row: what the human asked for, how far it got, and where it is now.
#[derive(Debug, Clone, FromRow)]
pub struct TaskRow {
    pub id: String,
    pub session_id: String,
    pub title: String,
    /// The goal loop's condition; `None` = single-turn task.
    pub goal_condition: Option<String>,
    /// pending / running / suspended / done / failed / cancelled.
    pub status: String,
    pub rounds_used: i32,
    pub max_rounds: i32,
    /// The turn carrying this task right now (a heartbeat; cleared when it ends).
    pub current_turn_id: Option<String>,
    pub suspend_reason: Option<String>,
    pub created_at: String,
    pub finished_at: Option<String>,
}

const COLS: &str = "id, session_id, title, goal_condition, status, rounds_used, max_rounds, \
                    current_turn_id, suspend_reason, created_at, finished_at";

/// The task store. Clone is cheap (one pool handle).
#[derive(Clone)]
pub struct Tasks {
    pool: SqlitePool,
}

impl Tasks {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Insert a new task. The partial unique index refuses a second active one; the caller
    /// checks first (for a human-readable message), this is the backstop.
    pub async fn create(
        &self,
        id: &str,
        session_id: &str,
        title: &str,
        goal_condition: Option<&str>,
        max_rounds: i32,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO agent_tasks (id, session_id, title, goal_condition, max_rounds)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(session_id)
        .bind(title)
        .bind(goal_condition)
        .bind(max_rounds)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> Result<Option<TaskRow>> {
        Ok(
            sqlx::query_as::<_, TaskRow>(&format!("SELECT {COLS} FROM agent_tasks WHERE id = ?"))
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// The session's one non-terminal task, when there is one.
    pub async fn active(&self, session_id: &str) -> Result<Option<TaskRow>> {
        Ok(sqlx::query_as::<_, TaskRow>(&format!(
            "SELECT {COLS} FROM agent_tasks
             WHERE session_id = ? AND status IN ('pending', 'running', 'suspended')
             ORDER BY created_at DESC LIMIT 1"
        ))
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// The task a turn is currently carrying (the heartbeat side of `current_turn_id`).
    pub async fn by_turn(&self, turn_id: &str) -> Result<Option<TaskRow>> {
        Ok(sqlx::query_as::<_, TaskRow>(&format!(
            "SELECT {COLS} FROM agent_tasks WHERE current_turn_id = ?"
        ))
        .bind(turn_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Every non-terminal task, across sessions — boot restores their goals into the runner.
    pub async fn list_active(&self) -> Result<Vec<TaskRow>> {
        Ok(sqlx::query_as::<_, TaskRow>(&format!(
            "SELECT {COLS} FROM agent_tasks
             WHERE status IN ('pending', 'running', 'suspended') ORDER BY created_at ASC"
        ))
        .fetch_all(&self.pool)
        .await?)
    }

    /// Newest first — the dashboard's task panel.
    pub async fn list(&self, limit: i64) -> Result<Vec<TaskRow>> {
        Ok(sqlx::query_as::<_, TaskRow>(&format!(
            "SELECT {COLS} FROM agent_tasks ORDER BY created_at DESC, rowid DESC LIMIT ?"
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// A turn starts carrying this task.
    pub async fn set_running(&self, id: &str, turn_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE agent_tasks SET status = 'running', current_turn_id = ?, suspend_reason = NULL,
             updated_at = datetime('now') WHERE id = ?",
        )
        .bind(turn_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The carrying turn ended (whatever its verdict); the row stays as it is.
    pub async fn clear_turn(&self, id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE agent_tasks SET current_turn_id = NULL, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The goal loop consumed another round — the human sees how much has been spent.
    pub async fn record_round(&self, id: &str, rounds: i32) -> Result<()> {
        sqlx::query(
            "UPDATE agent_tasks SET rounds_used = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(rounds)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// `goal/set`'s compatibility layer: change the condition on the live task.
    pub async fn set_goal(&self, id: &str, condition: Option<&str>) -> Result<()> {
        sqlx::query(
            "UPDATE agent_tasks SET goal_condition = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(condition)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Terminal state: done / failed / cancelled. Closes the row and stamps the time.
    pub async fn finish(&self, id: &str, status: &str) -> Result<()> {
        sqlx::query(
            "UPDATE agent_tasks SET status = ?, current_turn_id = NULL,
             finished_at = datetime('now'), updated_at = datetime('now') WHERE id = ?",
        )
        .bind(status)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// A human pressed pause: the reason is kept and shown, never dropped.
    pub async fn suspend(&self, id: &str, reason: Option<&str>) -> Result<()> {
        sqlx::query(
            "UPDATE agent_tasks SET status = 'suspended', suspend_reason = ?,
             updated_at = datetime('now') WHERE id = ?",
        )
        .bind(reason)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// A human pressed continue: back to running; the caller starts the turn.
    pub async fn resume(&self, id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE agent_tasks SET status = 'running', suspend_reason = NULL,
             updated_at = datetime('now') WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Startup reconciliation: a `running` row means the process died while carrying it
    /// (a `pending` one simply never got its first turn — leave it alone).
    /// Mark it suspended with the honest reason — but the turn's own checkpoint decides
    /// whether it actually continues (an `open` checkpoint resumes and flips it back).
    pub async fn reconcile_restart(&self) -> Result<Vec<TaskRow>> {
        let rows = sqlx::query_as::<_, TaskRow>(&format!(
            "SELECT {COLS} FROM agent_tasks WHERE status = 'running' ORDER BY created_at ASC"
        ))
        .fetch_all(&self.pool)
        .await?;
        sqlx::query(
            "UPDATE agent_tasks SET status = 'suspended',
             suspend_reason = '进程重启，未完成的回合若可续跑会自动继续',
             updated_at = datetime('now')
             WHERE status = 'running'",
        )
        .execute(&self.pool)
        .await?;
        Ok(rows)
    }
}
