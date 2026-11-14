//! The task ledger (migration 0017).
//!
//! A goal used to be a string in process memory: gone on restart, unsuspendable by nature.
//! 0014 made it a row; 0017 makes that row an **entity that stands on its own**:
//!
//! * a task may belong to no session at all (`session_id` is nullable);
//! * the same intent may be executed in several runs (the row outlives each run);
//! * "done" is decided by measurable facts — the row carries the acceptance conditions,
//!   the latest measured verdict, the budget, and how much has been spent.
//!
//! `needs_attention` is the honest middle state: the verifier failed, the budget ran out, or a
//! side effect never landed. It is **non-terminal** — a human can still confirm, continue, or
//! cancel it. `done` is never written on a "the model says so" basis alone.

use anyhow::Result;
use sqlx::sqlite::{Sqlite, SqliteArgumentValue, SqliteTypeInfo, SqliteValueRef};
use sqlx::{Decode, Encode, FromRow, SqlitePool, Type};

/// The lifecycle of a task. Terminal states close the row; `needs_attention` does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// Created, no run started yet.
    Pending,
    /// A run is carrying it right now.
    Running,
    /// A human pressed pause (or the process restarted mid-run).
    Suspended,
    /// Something a machine cannot settle: verification failed, budget spent, side effect unlanded.
    NeedsAttention,
    /// Acceptance conditions all passed (and the judge, if any, agreed).
    Done,
    /// The loop's own verdict: not achievable / round cap / the turn itself failed.
    Failed,
    /// A human stopped it. Never resumes on its own.
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Suspended => "suspended",
            Self::NeedsAttention => "needs_attention",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parse a stored value. `needsAttention` is accepted too: clients speak camelCase.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "suspended" => Some(Self::Suspended),
            "needs_attention" | "needsAttention" => Some(Self::NeedsAttention),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// Still alive: a human or a turn can push it forward. `needs_attention` is open on
    /// purpose — that is exactly the state where a person is expected to look.
    pub fn is_open(self) -> bool {
        matches!(
            self,
            Self::Pending | Self::Running | Self::Suspended | Self::NeedsAttention
        )
    }

    pub fn is_terminal(self) -> bool {
        !self.is_open()
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// Hand-written sqlx plumbing instead of `#[derive(sqlx::Type)]`: a derived decode turns an
// unknown stored value into a generic error; here the SQL `CHECK` constraint is the real
// guarantee, and the error names the offending value so a broken row is diagnosable.
impl Type<Sqlite> for TaskStatus {
    fn type_info() -> SqliteTypeInfo {
        <&str as Type<Sqlite>>::type_info()
    }
}

impl<'q> Encode<'q, Sqlite> for TaskStatus {
    fn encode_by_ref(
        &self,
        buf: &mut Vec<SqliteArgumentValue<'q>>,
    ) -> std::result::Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        <&str as Encode<Sqlite>>::encode(self.as_str(), buf)
    }
}

impl<'r> Decode<'r, Sqlite> for TaskStatus {
    fn decode(value: SqliteValueRef<'r>) -> std::result::Result<Self, sqlx::error::BoxDynError> {
        let raw = <&str as Decode<Sqlite>>::decode(value)?;
        Self::parse(raw).ok_or_else(|| format!("unknown task status: {raw}").into())
    }
}

/// One task row: what the human asked for, how it is judged, and how far it got.
#[derive(Debug, Clone, FromRow)]
pub struct TaskRow {
    pub id: String,
    /// `None` = a task that belongs to no conversation.
    pub session_id: Option<String>,
    pub title: String,
    /// The goal loop's condition; `None` = single-turn task.
    pub goal_condition: Option<String>,
    /// Acceptance conditions as JSON (`[{"kind":"fileExists","path":"report.md"}]`).
    pub acceptance_json: String,
    /// The latest measured verdict per condition, same order as `acceptance_json`.
    pub checks_json: String,
    /// Budget as JSON (`{"wallMs":…,"tokens":…,"tools":…}`); absent keys = no cap.
    pub budget_json: String,
    pub spent_wall_ms: i64,
    pub spent_tokens: i64,
    pub spent_tools: i64,
    /// The task's own working directory — the boundary for verification and writes.
    pub cwd: Option<String>,
    pub status: TaskStatus,
    pub rounds_used: i32,
    pub max_rounds: i32,
    /// The declared model tier (fast / chat / reasoning); `None` = decide per turn.
    pub tier: Option<String>,
    /// The turn carrying this task right now (a heartbeat; cleared when it ends).
    pub current_turn_id: Option<String>,
    pub suspend_reason: Option<String>,
    /// Why it stopped last time (verification failed / over budget / judge broke down).
    pub last_stop_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
}

const COLS: &str = "id, session_id, title, goal_condition, acceptance_json, checks_json, \
                    budget_json, spent_wall_ms, spent_tokens, spent_tools, cwd, status, \
                    rounds_used, max_rounds, tier, current_turn_id, suspend_reason, \
                    last_stop_reason, created_at, updated_at, finished_at";

/// What `create` needs: a struct instead of nine positional arguments — the call sites read
/// as "this task has these properties", and a new field cannot silently land in the wrong slot.
#[derive(Debug, Clone)]
pub struct NewTask<'a> {
    pub id: &'a str,
    pub session_id: Option<&'a str>,
    pub title: &'a str,
    pub goal_condition: Option<&'a str>,
    pub acceptance_json: &'a str,
    pub budget_json: &'a str,
    pub cwd: Option<&'a str>,
    pub max_rounds: i32,
    pub tier: Option<&'a str>,
}

/// The task store. Clone is cheap (one pool handle).
#[derive(Clone)]
pub struct Tasks {
    pool: SqlitePool,
}

impl Tasks {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Insert a new task. There is no "one active task per session" rule any more: the same
    /// session may hold several, and a task may hold no session at all.
    pub async fn create(&self, task: NewTask<'_>) -> Result<()> {
        sqlx::query(
            "INSERT INTO agent_tasks
                 (id, session_id, title, goal_condition, acceptance_json, budget_json,
                  cwd, max_rounds, tier)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(task.id)
        .bind(task.session_id)
        .bind(task.title)
        .bind(task.goal_condition)
        .bind(task.acceptance_json)
        .bind(task.budget_json)
        .bind(task.cwd)
        .bind(task.max_rounds)
        .bind(task.tier)
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

    /// The session's newest non-terminal task, when there is one. Several may be open —
    /// the card shows the newest, `task/list` shows all.
    pub async fn active(&self, session_id: &str) -> Result<Option<TaskRow>> {
        Ok(sqlx::query_as::<_, TaskRow>(&format!(
            "SELECT {COLS} FROM agent_tasks
             WHERE session_id = ? AND status IN ('pending', 'running', 'suspended', 'needs_attention')
             AND NOT EXISTS(SELECT 1 FROM task_dispatch d WHERE d.task_id=agent_tasks.id)
             AND NOT EXISTS(SELECT 1 FROM task_pause_fences p WHERE p.task_id=agent_tasks.id)
             ORDER BY created_at DESC, rowid DESC LIMIT 1"
        ))
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// The session's most recent task **whatever its state** — open ones beat closed ones,
    /// then newest first.
    ///
    /// `active` is for "what is running now"; the card needs "what happened here last".
    /// Using `active` there made a finished task vanish at the exact moment it became worth
    /// reading: the verdicts it passed and the files it produced are the answer to
    /// "did it actually do it", and they must survive `done`.
    pub async fn latest(&self, session_id: &str) -> Result<Option<TaskRow>> {
        Ok(sqlx::query_as::<_, TaskRow>(&format!(
            "SELECT {COLS} FROM agent_tasks
             WHERE session_id = ?
             ORDER BY CASE WHEN status IN ('pending', 'running', 'suspended', 'needs_attention')
                           THEN 0 ELSE 1 END,
                      created_at DESC, rowid DESC
             LIMIT 1"
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
             WHERE status IN ('pending', 'running', 'suspended', 'needs_attention')
             ORDER BY created_at ASC"
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

    /// A run starts carrying this task.
    pub async fn set_running(&self, id: &str, turn_id: &str) -> Result<()> {
        let changed=sqlx::query(
            "UPDATE agent_tasks SET status = 'running', current_turn_id = ?, suspend_reason = NULL,
             updated_at = datetime('now') WHERE id = ? AND NOT EXISTS(SELECT 1 FROM task_pause_fences WHERE task_id=agent_tasks.id)",
        )
        .bind(turn_id)
        .bind(id)
        .execute(&self.pool)
        .await?.rows_affected();
        anyhow::ensure!(changed == 1, "Task is unavailable or paused by the user");
        Ok(())
    }

    /// The carrying run ended (whatever its verdict); the row stays as it is.
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

    /// Accumulate what one round cost. Called once per round with that round's numbers —
    /// the row is the running total, so a restart never loses the spent half.
    pub async fn add_spent(&self, id: &str, wall_ms: i64, tokens: i64, tools: i64) -> Result<()> {
        sqlx::query(
            "UPDATE agent_tasks SET spent_wall_ms = spent_wall_ms + ?,
             spent_tokens = spent_tokens + ?, spent_tools = spent_tools + ?,
             updated_at = datetime('now') WHERE id = ?",
        )
        .bind(wall_ms.max(0))
        .bind(tokens.max(0))
        .bind(tools.max(0))
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Replace the latest measured verdict list (one JSON array, same order as the conditions).
    /// The `/goal` door: change the condition on the live task (the only place a goal lives).
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

    /// Terminal state: `done` / `failed` / `cancelled`. Closes the row and stamps the time.
    pub async fn finish(&self, id: &str, status: TaskStatus) -> Result<()> {
        self.finish_with_reason(id, status, None).await
    }

    pub async fn finish_with_reason(
        &self,
        id: &str,
        status: TaskStatus,
        reason: Option<&str>,
    ) -> Result<()> {
        self.finish_run(id, status, reason, None).await
    }

    /// Close an exact execution with its task and completion receipt in one commit.
    pub async fn finish_run(
        &self,
        id: &str,
        status: TaskStatus,
        reason: Option<&str>,
        run_id: Option<&str>,
    ) -> Result<()> {
        if !status.is_terminal() {
            return Err(anyhow::anyhow!(
                "finish only takes terminal states; got {status} (for anything else use suspend / needs_attention)"
            ));
        }
        let mut tx = self.pool.begin().await?;
        if let Some(run_id) = run_id {
            let run_status = match status {
                TaskStatus::Done => "completed",
                TaskStatus::Failed => "failed",
                TaskStatus::Cancelled => "cancelled",
                _ => unreachable!("terminal status checked above"),
            };
            let changed = sqlx::query("UPDATE task_runs SET status=?,stop_reason=?,ended_at=datetime('now') WHERE id=? AND task_id=?")
                .bind(run_status).bind(reason).bind(run_id).bind(id).execute(&mut *tx).await?.rows_affected();
            anyhow::ensure!(changed == 1, "Task completion run binding is missing");
        }

        if status == TaskStatus::Done {
            crate::verification::Verifications::require_completion(&mut tx, id, run_id).await?;
        }
        sqlx::query(
            "UPDATE agent_tasks SET status = ?, current_turn_id = NULL,suspend_reason=NULL,
             last_stop_reason=CASE WHEN ?='done' THEN ? ELSE COALESCE(?,last_stop_reason) END,
             finished_at = datetime('now'), updated_at = datetime('now') WHERE id = ?",
        )
        .bind(status)
        .bind(status)
        .bind(reason)
        .bind(reason)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// A human pressed pause: the reason is kept and shown, never dropped.
    pub async fn suspend(&self, id: &str, reason: Option<&str>) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO task_pause_fences(task_id) SELECT id FROM agent_tasks WHERE id=? ON CONFLICT(task_id) DO UPDATE SET paused_at=datetime('now')")
            .bind(id).execute(&mut *tx).await?;
        let changed=sqlx::query("UPDATE agent_tasks SET status='suspended',suspend_reason=?,last_stop_reason=?,updated_at=datetime('now') WHERE id=? AND status IN ('pending','running','suspended','needs_attention')")
            .bind(reason).bind(reason).bind(id).execute(&mut *tx).await?.rows_affected();
        anyhow::ensure!(changed == 1, "Task has ended or is unavailable for pause");
        sqlx::query(
            "UPDATE task_job_waits SET state='cancelled' WHERE task_id=? AND state='waiting'",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE task_input_waits SET state='cancelled' WHERE task_id=? AND state='waiting'",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE turn_checkpoints SET status='suspended',updated_at=datetime('now') WHERE status IN ('open','suspended','waiting_jobs','waiting_input') AND (run_id=(SELECT id FROM task_runs WHERE task_id=? ORDER BY started_at DESC,rowid DESC LIMIT 1) OR (run_id IS NULL AND turn_id=(SELECT current_turn_id FROM agent_tasks WHERE id=?)))")
            .bind(id).bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Something a machine must not decide alone: stopped, but still open for a human.
    /// Distinct from `suspend` so the card can show it in warning colours and say why.
    pub async fn needs_attention(&self, id: &str, reason: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE agent_tasks SET status = 'needs_attention', current_turn_id = NULL,
             suspend_reason = ?, last_stop_reason = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(reason)
        .bind(reason)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Why it stopped, without changing the status (used after a normal terminal write).
    pub async fn set_last_stop(&self, id: &str, reason: &str) -> Result<()> {
        sqlx::query("UPDATE agent_tasks SET last_stop_reason = ? WHERE id = ?")
            .bind(reason)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// A human pressed continue: back to running; the caller starts the run.
    pub async fn resume(&self, id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM task_pause_fences WHERE task_id=?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE agent_tasks SET status = 'running', suspend_reason = NULL,
             updated_at = datetime('now') WHERE id = ?",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Startup reconciliation: a `running` row means the process died while carrying it
    /// (a `pending` one simply never got its first run — leave it alone).
    /// Mark it suspended with the honest reason — but the run's own checkpoint decides
    /// whether it actually continues (an `open` checkpoint resumes and flips it back).
    pub async fn reconcile_restart(&self) -> Result<Vec<TaskRow>> {
        let rows = sqlx::query_as::<_, TaskRow>(&format!(
            "SELECT {COLS} FROM agent_tasks WHERE status = 'running' ORDER BY created_at ASC"
        ))
        .fetch_all(&self.pool)
        .await?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE agent_tasks SET status = 'suspended',\n             suspend_reason = 'the process restarted; an unfinished turn resumes automatically if it can',\n             last_stop_reason = 'the process restarted; an unfinished turn resumes automatically if it can',\n             updated_at = datetime('now')\n             WHERE status = 'running'",
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }
}
