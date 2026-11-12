//! The step ledger (migration 0017).
//!
//! A step is one unit of execution with an external effect. In P0 the granularity is **one tool
//! call** (`kind = "tool"`, `tool` = its name, `input_json` = its arguments) — that is where the
//! side effects live, so that is where idempotency has to sit. P1's planner adds `kind = "plan"`
//! steps; the shape does not change.
//!
//! The rule the whole crash story rests on: a step left `running` means "**sent, result never
//! landed**". On restart such a step becomes `waiting` — never replayed. A write that may already
//! have happened is a human's call, not a loop's guess.

use anyhow::Result;
use sqlx::{FromRow, SqlitePool};

/// One step row.
#[derive(Debug, Clone, FromRow)]
pub struct StepRow {
    pub id: i64,
    pub task_id: String,
    pub run_id: Option<String>,
    /// Position inside the run (1-based, monotonically increasing).
    pub seq: i32,
    /// `tool` (P0) / `plan` (P1).
    pub kind: String,
    pub tool: Option<String>,
    pub input_json: Option<String>,
    /// running / ok / failed / waiting / skipped.
    pub status: String,
    pub note: Option<String>,
    /// `<turn_id>:<seq>` — the same step of the same turn can only be recorded once.
    pub idempotency_key: Option<String>,
    /// A short digest of the tool's output (`ok`, the first line, a byte count).
    pub output_digest: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
}

const COLS: &str = "id, task_id, run_id, seq, kind, tool, input_json, status, note, \
                    idempotency_key, output_digest, started_at, ended_at";

/// A step about to run. A struct rather than eight positional arguments: the call site is the
/// one place that knows all of them, and order mistakes here would be silent.
#[derive(Debug, Clone)]
pub struct NewStep<'a> {
    pub task_id: &'a str,
    pub run_id: Option<&'a str>,
    pub seq: i32,
    pub kind: &'a str,
    pub tool: Option<&'a str>,
    pub input_json: Option<&'a str>,
    /// `<turn_id>:<seq>` — the idempotency key.
    pub idempotency_key: Option<&'a str>,
}

/// The step store. Clone is cheap (one pool handle).
#[derive(Clone)]
pub struct Steps {
    pool: SqlitePool,
}

impl Steps {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub fn plans(&self) -> crate::Plans {
        crate::Plans::new(self.pool.clone())
    }

    /// Record a step as `running` — "it is being sent right now". Returns the row id.
    ///
    /// A duplicate `idempotency_key` does not insert: it returns the existing row's id, so a
    /// re-entrant call can see that this step was already there instead of step counts drifting.
    pub async fn begin(&self, step: NewStep<'_>) -> Result<i64> {
        sqlx::query(
            "INSERT INTO task_steps (task_id, run_id, seq, kind, tool, input_json, idempotency_key)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(idempotency_key) WHERE idempotency_key IS NOT NULL DO NOTHING",
        )
        .bind(step.task_id)
        .bind(step.run_id)
        .bind(step.seq)
        .bind(step.kind)
        .bind(step.tool)
        .bind(step.input_json)
        .bind(step.idempotency_key)
        .execute(&self.pool)
        .await?;
        // Whether it inserted or not, the row for this key is the step.
        if let Some(key) = step.idempotency_key {
            if let Some(id) =
                sqlx::query_scalar::<_, i64>("SELECT id FROM task_steps WHERE idempotency_key = ?")
                    .bind(key)
                    .fetch_optional(&self.pool)
                    .await?
            {
                return Ok(id);
            }
        }
        // No key (or somehow gone): newest row of this run is the one we just wrote.
        let id: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM task_steps WHERE task_id = ?")
                .bind(step.task_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(id)
    }

    /// Close a step with its verdict. `status` is ok / failed / skipped / waiting.
    pub async fn close(
        &self,
        id: i64,
        status: &str,
        output_digest: Option<&str>,
        note: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE task_steps SET status = ?, output_digest = ?, note = ?,
             ended_at = datetime('now') WHERE id = ?",
        )
        .bind(status)
        .bind(output_digest)
        .bind(note)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The task's plan steps, oldest version first — the readable plan (P1).
    ///
    /// They live outside any run on purpose: a plan outlives the execution that produced it, and
    /// the next run (or the next process) reads the same rows.
    pub async fn list_plan(&self, task_id: &str) -> Result<Vec<StepRow>> {
        Ok(sqlx::query_as::<_, StepRow>(&format!(
            "SELECT {COLS} FROM task_steps WHERE task_id = ? AND kind = 'plan'
             ORDER BY id ASC"
        ))
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// One run's steps, in order — the readable execution trace.
    pub async fn list_by_run(&self, run_id: &str) -> Result<Vec<StepRow>> {
        Ok(sqlx::query_as::<_, StepRow>(&format!(
            "SELECT {COLS} FROM task_steps WHERE run_id = ? ORDER BY id ASC"
        ))
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Startup reconciliation: steps left `running` mean "sent, result never landed". They
    /// become `waiting` with the honest reason — a human confirms before anything is re-sent.
    /// Returns how many were turned.
    pub async fn settle_running(&self, reason: &str) -> Result<u64> {
        let result = sqlx::query(
            "UPDATE task_steps SET status = 'waiting', note = ?, ended_at = datetime('now')
             WHERE status = 'running'",
        )
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// How many steps of this task already succeeded — `ToolsOk` acceptance reads this
    /// (it counts what actually ran, not what the model claims).
    /// Evidence for the current execution, excluding successful calls from older runs.
    pub async fn count_ok_by_tool_in_run(
        &self,
        task_id: &str,
        run_id: &str,
        tool: &str,
    ) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM task_steps WHERE task_id = ? AND run_id = ? AND tool = ? AND status = 'ok'")
            .bind(task_id).bind(run_id).bind(tool).fetch_one(&self.pool).await?)
    }

    pub async fn count_ok_by_tool(&self, task_id: &str, tool: &str) -> Result<i64> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM task_steps WHERE task_id = ? AND tool = ? AND status = 'ok'",
        )
        .bind(task_id)
        .bind(tool)
        .fetch_one(&self.pool)
        .await?)
    }
}
