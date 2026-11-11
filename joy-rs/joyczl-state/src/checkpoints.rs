//! The turn-level checkpoint ledger (migration 0013, extended by 0017).
//!
//! A turn's working memory (`Vec<Message>` — assistant thoughts, tool calls, tool results)
//! normally lives only in process memory. This store writes it to disk at each tool-results
//! boundary, so a fresh process can resume the loop instead of replaying finished tools.
//! Unlike the jobs ledger (fact-only, reconcile marks without rerunning), this holds
//! **resumable state**: startup reconciliation resumes `open` rows automatically.
//!
//! Since 0017 a checkpoint also says **which run** it belongs to: resuming a task's turn is
//! resuming one attempt, and its steps are reconciled together with it.

use anyhow::Result;
use sqlx::{FromRow, SqlitePool};

/// One checkpoint row: a turn's durable progress.
#[derive(Debug, Clone, FromRow)]
pub struct CheckpointRow {
    pub turn_id: String,
    pub session_id: String,
    pub user_message: String,
    /// `Vec<Message>` serialized — the loop's working memory at the last boundary.
    pub messages_json: String,
    /// How many loop iterations are already done.
    pub iterations: i32,
    /// Token usage spent before the checkpoint (resumed turns add both halves up).
    pub usage_json: String,
    /// `open` / `done` / `abandoned` / `suspended`.
    pub status: String,
    /// The run (task attempt) this checkpoint belongs to; `None` for a task-less turn.
    pub run_id: Option<String>,
    /// The boundary this checkpoint lands on. In P0 steps are tool calls, so this is the
    /// completed-iterations index; P1's plan steps will refine it.
    pub step_seq: i32,
    /// The plan version it belongs to (P1); `None` until a planner exists.
    pub plan_version: Option<i64>,
    pub updated_at: String,
}

/// The checkpoint store. Clone is cheap (one pool handle).
#[derive(Clone)]
pub struct Checkpoints {
    pool: SqlitePool,
}

impl Checkpoints {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Upsert one checkpoint: every boundary overwrites the previous one — only the newest
    /// working memory is worth resuming from.
    #[allow(clippy::too_many_arguments)]
    pub async fn save(
        &self,
        turn_id: &str,
        session_id: &str,
        user_message: &str,
        messages_json: &str,
        iterations: i32,
        usage_json: &str,
        run_id: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO turn_checkpoints
                 (turn_id, session_id, user_message, messages_json, iterations, usage_json,
                  run_id, step_seq)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(turn_id) DO UPDATE SET
                 session_id = excluded.session_id,
                 user_message = excluded.user_message,
                 messages_json = excluded.messages_json,
                 iterations = excluded.iterations,
                 usage_json = excluded.usage_json,
                 run_id = excluded.run_id,
                 step_seq = excluded.step_seq,
                 status = 'open',
                 updated_at = datetime('now')",
        )
        .bind(turn_id)
        .bind(session_id)
        .bind(user_message)
        .bind(messages_json)
        .bind(iterations)
        .bind(usage_json)
        .bind(run_id)
        .bind(iterations)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Turn reached an end: `status` is `done` (finished normally) or `abandoned`
    /// (the human interrupted — the checkpoint stays for the ledger, never auto-resumed).
    pub async fn finish(&self, turn_id: &str, status: &str) -> Result<()> {
        sqlx::query("UPDATE turn_checkpoints SET status = ?, updated_at = datetime('now') WHERE turn_id = ?")
            .bind(status)
            .bind(turn_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// A suspended checkpoint the human chose to continue: back to `open`, so every further
    /// boundary overwrites it normally and a crash during the resume is recovered like any other.
    pub async fn reopen(&self, turn_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE turn_checkpoints SET status = 'open', updated_at = datetime('now') WHERE turn_id = ?",
        )
        .bind(turn_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// One checkpoint by turn id — `task/resume` looks up the `suspended` row it wants to continue.
    pub async fn get(&self, turn_id: &str) -> Result<Option<CheckpointRow>> {
        Ok(sqlx::query_as::<_, CheckpointRow>(
            "SELECT turn_id, session_id, user_message, messages_json, iterations, usage_json,
                    status, run_id, step_seq, plan_version, updated_at
             FROM turn_checkpoints WHERE turn_id = ?",
        )
        .bind(turn_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Startup reconciliation: all `open` checkpoints (leftovers of the last crash),
    /// oldest first — a resumed turn may itself checkpoint, so consume in order.
    /// `rowid` not `updated_at`: two checkpoints inside the same second must still keep
    /// insertion order (SQLite's datetime has one-second resolution).
    pub async fn open_rows(&self) -> Result<Vec<CheckpointRow>> {
        Ok(sqlx::query_as::<_, CheckpointRow>(
            "SELECT turn_id, session_id, user_message, messages_json, iterations, usage_json,
                    status, run_id, step_seq, plan_version, updated_at
             FROM turn_checkpoints WHERE status = 'open' ORDER BY rowid ASC",
        )
        .fetch_all(&self.pool)
        .await?)
    }
}
