//! The run ledger (migration 0017).
//!
//! A **run** is one execution of a task: it opens before a turn's work starts and closes when
//! that work ends. A task may be executed several times (a human pressed continue, a pause was
//! resumed, a restart re-opened it) — the runs are the account of each attempt, the task row is
//! the intent.
//!
//! `running` on boot means the process died while carrying that run. Reconciliation closes it
//! as `waiting` — **not** `failed` and never "replayed": the steps inside it decide, one by one,
//! whether a side effect landed (see `steps.rs`), and a human gets the final say.

use anyhow::Result;
use sqlx::{FromRow, SqlitePool};

/// The lifecycle of one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    /// Stopped at a boundary a machine cannot settle: side effect unlanded, human needed.
    Waiting,
    Suspended,
    Completed,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Suspended => "suspended",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One run row. `status` stays text: unlike the task row, nothing branches on it — it is the
/// account the human reads and the reconciler matches (`WHERE status = 'running'`).
#[derive(Debug, Clone, FromRow)]
pub struct RunRow {
    pub id: String,
    pub task_id: String,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub status: String,
    pub rounds: i32,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub tools_ok: i64,
    pub tools_failed: i64,
    pub stop_reason: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
}

const COLS: &str = "id, task_id, session_id, turn_id, status, rounds, tokens_in, tokens_out, \
                    tools_ok, tools_failed, stop_reason, started_at, ended_at";

/// Cumulative observations of one stable loop round (including resumed slices).
#[derive(Debug, Clone, Default, sqlx::FromRow)]
pub struct RoundAccount {
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub tools_ok: i64,
    pub tools_failed: i64,
    pub wall_ms: i64,
}

/// The run store. Clone is cheap (one pool handle).
#[derive(Clone)]
pub struct Runs {
    pool: SqlitePool,
}

impl Runs {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Open a run. `id` is chosen by the caller: `<turn_id>#<round>` — unique by construction,
    /// and the id itself says which turn and which round it belongs to.
    pub async fn start(
        &self,
        id: &str,
        task_id: &str,
        session_id: Option<&str>,
        turn_id: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO task_runs (id, task_id, session_id, turn_id) VALUES (?, ?, ?, ?)
             ON CONFLICT(id) DO NOTHING",
        )
        .bind(id)
        .bind(task_id)
        .bind(session_id)
        .bind(turn_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Restore the same interrupted attempt without erasing its counters.
    /// A terminal attempt cannot be reopened by a stale checkpoint.
    pub async fn reopen(&self, id: &str) -> Result<bool> {
        Ok(sqlx::query(
            "UPDATE task_runs SET status = 'running', ended_at = NULL, stop_reason = NULL
             WHERE id = ? AND status IN ('running', 'waiting', 'suspended')",
        )
        .bind(id)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Add one round's numbers to the run's account. Called at every round boundary, so a
    /// crash leaves "how much this run already spent" behind.
    pub async fn add_round(
        &self,
        id: &str,
        tokens_in: i64,
        tokens_out: i64,
        tools_ok: i64,
        tools_failed: i64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE task_runs SET rounds = rounds + 1, tokens_in = tokens_in + ?,
             tokens_out = tokens_out + ?, tools_ok = tools_ok + ?, tools_failed = tools_failed + ?
             WHERE id = ?",
        )
        .bind(tokens_in.max(0))
        .bind(tokens_out.max(0))
        .bind(tools_ok.max(0))
        .bind(tools_failed.max(0))
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn round_account(&self, run_id: &str, round_id: &str) -> Result<RoundAccount> {
        Ok(sqlx::query_as("SELECT tokens_in, tokens_out, tools_ok, tools_failed, wall_ms FROM run_round_accounts WHERE run_id = ? AND round_id = ?")
            .bind(run_id).bind(round_id).fetch_optional(&self.pool).await?.unwrap_or_default())
    }

    /// Task totals, run totals and the round watermark commit together. Re-observing a
    /// recovered round charges only the new cumulative delta, never its checkpointed prefix.
    pub async fn account_round(
        &self,
        run_id: &str,
        round_id: &str,
        current: &RoundAccount,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        // Acquire the write lock before reading watermarks, including for concurrent callers.
        sqlx::query("INSERT INTO run_round_accounts (run_id, round_id) VALUES (?, ?) ON CONFLICT DO NOTHING")
            .bind(run_id).bind(round_id).execute(&mut *tx).await?;
        let previous: RoundAccount = sqlx::query_as("SELECT tokens_in, tokens_out, tools_ok, tools_failed, wall_ms FROM run_round_accounts WHERE run_id = ? AND round_id = ?")
            .bind(run_id).bind(round_id).fetch_one(&mut *tx).await?;
        let input = current.tokens_in.saturating_sub(previous.tokens_in).max(0);
        let output = current
            .tokens_out
            .saturating_sub(previous.tokens_out)
            .max(0);
        let ok = current.tools_ok.saturating_sub(previous.tools_ok).max(0);
        let failed = current
            .tools_failed
            .saturating_sub(previous.tools_failed)
            .max(0);
        let wall = current.wall_ms.saturating_sub(previous.wall_ms).max(0);
        sqlx::query("UPDATE agent_tasks SET spent_tokens = spent_tokens + ?, spent_tools = spent_tools + ?, spent_wall_ms = spent_wall_ms + ?, updated_at = datetime('now') WHERE id = (SELECT task_id FROM task_runs WHERE id = ?)")
            .bind(input.saturating_add(output)).bind(ok.saturating_add(failed)).bind(wall).bind(run_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE task_runs SET tokens_in = tokens_in + ?, tokens_out = tokens_out + ?, tools_ok = tools_ok + ?, tools_failed = tools_failed + ?, rounds = (SELECT COUNT(*) FROM run_round_accounts WHERE run_id = ?) WHERE id = ?")
            .bind(input).bind(output).bind(ok).bind(failed).bind(run_id).bind(run_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE run_round_accounts SET tokens_in = MAX(tokens_in, ?), tokens_out = MAX(tokens_out, ?), tools_ok = MAX(tools_ok, ?), tools_failed = MAX(tools_failed, ?), wall_ms = MAX(wall_ms, ?) WHERE run_id = ? AND round_id = ?")
            .bind(current.tokens_in.max(0)).bind(current.tokens_out.max(0)).bind(current.tools_ok.max(0)).bind(current.tools_failed.max(0)).bind(current.wall_ms.max(0)).bind(run_id).bind(round_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Close a run with its verdict and (optional) reason. Idempotent: re-closing overwrites.
    pub async fn close(&self, id: &str, status: RunStatus, reason: Option<&str>) -> Result<()> {
        sqlx::query(
            "UPDATE task_runs SET status = ?, stop_reason = ?, ended_at = datetime('now')
             WHERE id = ?",
        )
        .bind(status.as_str())
        .bind(reason)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> Result<Option<RunRow>> {
        Ok(
            sqlx::query_as::<_, RunRow>(&format!("SELECT {COLS} FROM task_runs WHERE id = ?"))
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Preserve terminal outcomes when an older unfinished attempt is superseded.
    pub async fn supersede(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE task_runs SET status='suspended',stop_reason='Superseded by a newer task execution',ended_at=datetime('now') WHERE id=? AND status IN ('running','waiting','suspended')")
            .bind(id).execute(&self.pool).await?;
        Ok(())
    }

    /// Resolve execution identity without assuming run and turn identifiers are equal.
    pub async fn for_turn(&self, turn_id: &str) -> Result<Option<RunRow>> {
        let mut rows = sqlx::query_as::<_, RunRow>(&format!(
            "SELECT {COLS} FROM task_runs WHERE turn_id=? OR (turn_id IS NULL AND id=?) LIMIT 2"
        ))
        .bind(turn_id)
        .bind(turn_id)
        .fetch_all(&self.pool)
        .await?;
        anyhow::ensure!(rows.len() <= 1, "Turn has ambiguous task run bindings");
        Ok(rows.pop())
    }

    /// One task's runs, newest first — the card's "executed N times" and the detail panel.
    pub async fn list_by_task(&self, task_id: &str, limit: i64) -> Result<Vec<RunRow>> {
        Ok(sqlx::query_as::<_, RunRow>(&format!(
            "SELECT {COLS} FROM task_runs WHERE task_id = ?
             ORDER BY started_at DESC, rowid DESC LIMIT ?"
        ))
        .bind(task_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// How many times this task has been executed (finished or not).
    pub async fn count_by_task(&self, task_id: &str) -> Result<i64> {
        Ok(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM task_runs WHERE task_id = ?")
                .bind(task_id)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// Startup reconciliation: runs left `running` belong to a dead process. Oldest first —
    /// the caller needs them in order to walk their steps.
    pub async fn open_runs(&self) -> Result<Vec<RunRow>> {
        Ok(sqlx::query_as::<_, RunRow>(&format!(
            "SELECT {COLS} FROM task_runs WHERE status = 'running' ORDER BY started_at ASC, rowid ASC"
        ))
        .fetch_all(&self.pool)
        .await?)
    }
}
