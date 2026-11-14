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
    pub round_id: Option<String>,
    pub round_no: Option<i32>,
    pub turn_id: String,
    pub session_id: String,
    pub user_message: String,
    /// `Vec<Message>` serialized — the loop's working memory at the last boundary.
    pub messages_json: String,
    /// How many loop iterations are already done.
    pub iterations: i32,
    /// Token usage spent before the checkpoint (resumed turns add both halves up).
    pub usage_json: String,
    /// `open` / `done` / `abandoned` / `suspended` / `waiting_jobs`.
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

/// Borrowed working-memory snapshot written atomically with its execution identity.
pub struct CheckpointWrite<'a> {
    pub turn_id: &'a str,
    pub session_id: &'a str,
    pub user_message: &'a str,
    pub messages_json: &'a str,
    pub iterations: i32,
    pub usage_json: &'a str,
    pub run_id: Option<&'a str>,
    pub round_id: Option<&'a str>,
    pub round_no: Option<i32>,
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
        self.save_round(
            turn_id,
            session_id,
            user_message,
            messages_json,
            iterations,
            usage_json,
            run_id,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn save_round(
        &self,
        turn_id: &str,
        session_id: &str,
        user_message: &str,
        messages_json: &str,
        iterations: i32,
        usage_json: &str,
        run_id: Option<&str>,
        round_id: Option<&str>,
    ) -> Result<()> {
        self.save_numbered_round(CheckpointWrite {
            turn_id,
            session_id,
            user_message,
            messages_json,
            iterations,
            usage_json,
            run_id,
            round_id,
            round_no: None,
        })
        .await
    }

    /// Commit round admission and working memory together before any execution.
    pub async fn save_numbered_round(&self, checkpoint: CheckpointWrite<'_>) -> Result<()> {
        self.save_with_operations(checkpoint, &[]).await
    }

    /// Operation intent/results and the resumable boundary share one commit.
    pub async fn save_with_operations(
        &self,
        checkpoint: CheckpointWrite<'_>,
        operations: &[crate::operations::OperationSnapshot],
    ) -> Result<()> {
        let CheckpointWrite {
            turn_id,
            session_id,
            user_message,
            messages_json,
            iterations,
            usage_json,
            run_id,
            round_id,
            round_no,
        } = checkpoint;
        anyhow::ensure!(
            round_no.is_none_or(|number| number > 0),
            "Invalid checkpoint round number"
        );
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO turn_checkpoints
                 (turn_id, session_id, user_message, messages_json, iterations, usage_json,
                  run_id, step_seq, round_id, round_no)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(turn_id) DO UPDATE SET
                 session_id = excluded.session_id,
                 user_message = excluded.user_message,
                 messages_json = excluded.messages_json,
                 iterations = excluded.iterations,
                 usage_json = excluded.usage_json,
                 run_id = excluded.run_id,
                 step_seq = excluded.step_seq,
                 round_id = excluded.round_id,
                 round_no = excluded.round_no,
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
        .bind(round_id)
        .bind(round_no)
        .execute(&mut *tx)
        .await?;
        if let (Some(run), Some(number)) = (run_id, round_no) {
            let changed = sqlx::query("UPDATE agent_tasks SET rounds_used = ?, updated_at=datetime('now') WHERE id=(SELECT task_id FROM task_runs WHERE id=?)")
                .bind(number).bind(run).execute(&mut *tx).await?.rows_affected();
            anyhow::ensure!(changed == 1, "Checkpoint run has no task");
        }
        if !operations.is_empty() {
            let round = round_id
                .ok_or_else(|| anyhow::anyhow!("Operation checkpoint has no round identity"))?;
            crate::operations::checkpoint(
                &mut tx,
                turn_id,
                session_id,
                run_id,
                round,
                "checkpoint",
                operations,
            )
            .await?;
        }
        tx.commit().await?;
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

    /// Recovery failures cannot reopen terminal tasks, revoke pauses, or stop a newer run.
    pub async fn review(&self, turn_id: &str, run_id: Option<&str>, reason: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        if let Some(run_id) = run_id {
            let changed=sqlx::query("UPDATE agent_tasks SET status='needs_attention',suspend_reason=?,last_stop_reason=?,current_turn_id=NULL,updated_at=datetime('now')
                WHERE status IN ('pending','running','suspended','needs_attention')
                AND (current_turn_id IS NULL OR current_turn_id=?)
                AND NOT EXISTS(SELECT 1 FROM task_pause_fences p WHERE p.task_id=agent_tasks.id)
                AND id=(SELECT r.task_id FROM task_runs r JOIN turn_checkpoints c ON c.run_id=r.id
                    WHERE c.turn_id=? AND r.id=? AND (r.session_id IS NULL OR r.session_id=c.session_id)
                    AND (r.turn_id=c.turn_id OR (r.turn_id IS NULL AND r.id=c.turn_id))
                    AND r.id=(SELECT latest.id FROM task_runs latest WHERE latest.task_id=r.task_id ORDER BY latest.started_at DESC,latest.rowid DESC LIMIT 1))")
                .bind(reason).bind(reason).bind(turn_id).bind(turn_id).bind(run_id).execute(&mut *tx).await?.rows_affected();
            if changed == 1 {
                sqlx::query("UPDATE task_runs SET status='waiting',stop_reason=?,ended_at=datetime('now') WHERE id=? AND status IN ('running','waiting','suspended')")
                    .bind(reason).bind(run_id).execute(&mut *tx).await?;
            }
        }
        sqlx::query("UPDATE turn_checkpoints SET status='suspended',updated_at=datetime('now') WHERE turn_id=? AND status IN ('open','suspended','waiting_jobs','waiting_input')")
            .bind(turn_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// One checkpoint by turn id — `task/resume` looks up the `suspended` row it wants to continue.
    pub async fn get(&self, turn_id: &str) -> Result<Option<CheckpointRow>> {
        Ok(sqlx::query_as::<_, CheckpointRow>(
            "SELECT turn_id, session_id, user_message, messages_json, iterations, usage_json,
                    status, run_id, step_seq, plan_version, updated_at, round_id, round_no
             FROM turn_checkpoints WHERE turn_id = ?",
        )
        .bind(turn_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Recoverable ordinary-chat boundaries, including legacy rows without an operation journal.
    pub async fn chat_boundaries(&self, session: &str) -> Result<Vec<CheckpointRow>> {
        Ok(sqlx::query_as("SELECT * FROM turn_checkpoints WHERE session_id=? AND run_id IS NULL AND status IN ('open','suspended','abandoned') ORDER BY rowid")
            .bind(session).fetch_all(&self.pool).await?)
    }

    /// Human review may stop an unclaimed uncertain boundary without authorizing replay.
    pub async fn stop_chat_review(&self, row: &CheckpointRow) -> Result<bool> {
        Ok(sqlx::query("UPDATE turn_checkpoints SET status='suspended' WHERE turn_id=? AND session_id=? AND run_id IS NULL AND status='open' AND messages_json=?")
            .bind(&row.turn_id).bind(&row.session_id).bind(&row.messages_json).execute(&self.pool).await?.rows_affected() == 1)
    }

    /// Startup reconciliation: all `open` checkpoints (leftovers of the last crash),
    /// oldest first — a resumed turn may itself checkpoint, so consume in order.
    /// `rowid` not `updated_at`: two checkpoints inside the same second must still keep
    /// insertion order (SQLite's datetime has one-second resolution).
    pub async fn open_rows(&self) -> Result<Vec<CheckpointRow>> {
        Ok(sqlx::query_as::<_, CheckpointRow>(
            "SELECT turn_id, session_id, user_message, messages_json, iterations, usage_json,
                    status, run_id, step_seq, plan_version, updated_at, round_id, round_no
             FROM turn_checkpoints WHERE status = 'open' ORDER BY rowid ASC",
        )
        .fetch_all(&self.pool)
        .await?)
    }
}
