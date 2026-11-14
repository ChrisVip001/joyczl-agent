//! Durable, source-bound input obligations for tasks already authorized by humans.
use anyhow::{ensure, Result};
use sqlx::{FromRow, SqlitePool};

pub const INPUT_WAIT_REASON: &str = "waiting for authorized external input";

#[derive(Debug, Clone, FromRow)]
pub struct InputCondition {
    pub condition_id: String,
    pub task_id: String,
    pub source: String,
    pub event_key: String,
    pub description: String,
    pub expires_at: Option<String>,
    pub event_id: Option<String>,
    pub payload_json: Option<String>,
    pub received_at: Option<String>,
    pub consumed_run_id: Option<String>,
}
const CONDITION_COLS: &str = "condition_id,task_id,source,event_key,description,expires_at,event_id,payload_json,received_at,consumed_run_id";

pub struct NewInputCondition<'a> {
    pub condition_id: &'a str,
    pub task_id: &'a str,
    pub source: &'a str,
    pub event_key: &'a str,
    pub description: &'a str,
    pub expires_at: Option<&'a str>,
}
#[derive(Debug, Clone, FromRow)]
pub struct InputWait {
    pub task_id: String,
    pub condition_id: String,
    pub run_id: String,
    pub turn_id: String,
    pub session_id: String,
    pub state: String,
}
pub struct WaitSnapshot<'a> {
    pub messages_json: &'a str,
    pub usage_json: &'a str,
    pub iterations: i32,
    pub round_id: &'a str,
    pub round_no: i32,
}
#[derive(Clone)]
pub struct ExternalWaits {
    pool: SqlitePool,
}
impl ExternalWaits {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn define(&self, input: NewInputCondition<'_>) -> Result<()> {
        for (name, value, cap) in [
            ("condition id", input.condition_id, 256),
            ("source", input.source, 256),
            ("event key", input.event_key, 256),
            ("description", input.description, 4096),
        ] {
            ensure!(
                !value.trim().is_empty() && value.len() <= cap,
                "Invalid input {name}"
            );
        }
        let mut tx = self.pool.begin().await?;
        let task = sqlx::query("UPDATE agent_tasks SET updated_at=datetime('now') WHERE id=? AND status='pending' AND current_turn_id IS NULL AND NOT EXISTS(SELECT 1 FROM task_runs WHERE task_id=agent_tasks.id)")
            .bind(input.task_id).execute(&mut *tx).await?.rows_affected();
        ensure!(
            task == 1,
            "Input conditions must be declared before the task starts"
        );
        if let Some(expires) = input.expires_at {
            let valid: bool =
                sqlx::query_scalar("SELECT COALESCE(julianday(?) > julianday('now'),0)")
                    .bind(expires)
                    .fetch_one(&mut *tx)
                    .await?;
            ensure!(valid, "Input deadline must be a valid future timestamp");
        }
        sqlx::query("INSERT INTO task_input_conditions(condition_id,task_id,source,event_key,description,expires_at) VALUES(?,?,?,?,?,?)")
            .bind(input.condition_id).bind(input.task_id).bind(input.source).bind(input.event_key).bind(input.description).bind(input.expires_at).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn get(&self, task_id: &str) -> Result<Option<InputCondition>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {CONDITION_COLS} FROM task_input_conditions WHERE task_id=?"
        ))
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?)
    }
    pub async fn expired(&self, task_id: &str) -> Result<bool> {
        let state: Option<i32>=sqlx::query_scalar("SELECT CASE WHEN expires_at IS NULL THEN 0 WHEN julianday(expires_at) IS NULL THEN -1 WHEN event_id IS NOT NULL THEN 0 WHEN julianday(expires_at)<=julianday('now') THEN 1 ELSE 0 END FROM task_input_conditions WHERE task_id=?")
            .bind(task_id).fetch_optional(&self.pool).await?;
        ensure!(
            state != Some(-1),
            "Input deadline is invalid; human review is required"
        );
        Ok(state == Some(1))
    }

    /// A duplicate is acknowledged only if it names the same condition and immutable data.
    pub async fn signal(
        &self,
        condition_id: &str,
        source: &str,
        event_key: &str,
        event_id: &str,
        payload_json: &str,
    ) -> Result<bool> {
        ensure!(
            !event_id.trim().is_empty() && event_id.len() <= 256,
            "Invalid input event id"
        );
        ensure!(
            payload_json.len() <= 16_384,
            "Input payload exceeds 16384 bytes"
        );
        let payload: serde_json::Value = serde_json::from_str(payload_json)?;
        let mut tx = self.pool.begin().await?;
        let locked = sqlx::query(
            "UPDATE task_input_conditions SET condition_id=condition_id WHERE condition_id=?",
        )
        .bind(condition_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        ensure!(locked == 1, "Input condition is unavailable");
        let condition: InputCondition = sqlx::query_as(&format!(
            "SELECT {CONDITION_COLS} FROM task_input_conditions WHERE condition_id=?"
        ))
        .bind(condition_id)
        .fetch_one(&mut *tx)
        .await?;
        ensure!(
            condition.source == source && condition.event_key == event_key,
            "Input source or correlation key does not match"
        );
        let existing: Option<(String, String)> = sqlx::query_as(
            "SELECT condition_id,payload_json FROM task_input_events WHERE source=? AND event_id=?",
        )
        .bind(source)
        .bind(event_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((original, body)) = existing {
            ensure!(
                original == condition_id
                    && serde_json::from_str::<serde_json::Value>(&body)? == payload,
                "Input event id was already used for different data"
            );
            tx.commit().await?;
            return Ok(false);
        }
        let active: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM agent_tasks WHERE id=? AND status IN ('pending','running','suspended','needs_attention'))")
            .bind(&condition.task_id).fetch_one(&mut *tx).await?;
        ensure!(active, "Input task has ended");
        ensure!(
            condition.event_id.is_none(),
            "Input condition already has an immutable result"
        );
        let expired: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM task_input_conditions WHERE condition_id=? AND expires_at IS NOT NULL AND COALESCE(julianday(expires_at)<=julianday('now'),1))")
            .bind(condition_id).fetch_one(&mut *tx).await?;
        ensure!(
            !expired,
            "Input condition has expired; human review is required"
        );
        sqlx::query("INSERT INTO task_input_events(source,event_id,condition_id,payload_json) VALUES(?,?,?,?)")
            .bind(source).bind(event_id).bind(condition_id).bind(payload_json).execute(&mut *tx).await?;
        sqlx::query("UPDATE task_input_conditions SET event_id=?,payload_json=?,received_at=datetime('now') WHERE condition_id=?")
            .bind(event_id).bind(payload_json).bind(condition_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }
    /// Only data actually included in this run's prompt can satisfy its input obligation.
    pub async fn for_prompt(&self, task_id: &str, run_id: &str) -> Result<Option<InputCondition>> {
        if self.get(task_id).await?.is_none() {
            return Ok(None);
        }
        let mut tx = self.pool.begin().await?;
        let execution=sqlx::query("UPDATE task_runs SET id=id WHERE id=? AND task_id=? AND status='running' AND EXISTS(SELECT 1 FROM agent_tasks t WHERE t.id=task_runs.task_id AND t.status='running')")
            .bind(run_id).bind(task_id).execute(&mut *tx).await?.rows_affected();
        ensure!(
            execution == 1,
            "Input prompt has no active task execution binding"
        );

        sqlx::query("UPDATE task_input_conditions SET consumed_run_id=? WHERE task_id=? AND event_id IS NOT NULL AND EXISTS(SELECT 1 FROM task_runs r JOIN agent_tasks t ON t.id=r.task_id WHERE r.id=? AND t.id=task_input_conditions.task_id AND t.status='running')")
            .bind(run_id).bind(task_id).bind(run_id).execute(&mut *tx).await?;
        let row = sqlx::query_as(&format!(
            "SELECT {CONDITION_COLS} FROM task_input_conditions WHERE task_id=?"
        ))
        .bind(task_id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(row)
    }
    pub async fn register(&self, wait: &InputWait, snapshot: WaitSnapshot<'_>) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let task=sqlx::query("UPDATE agent_tasks SET status='suspended',suspend_reason=?,last_stop_reason=?,updated_at=datetime('now') WHERE id=? AND status='running' AND current_turn_id=?")
            .bind(INPUT_WAIT_REASON).bind(INPUT_WAIT_REASON).bind(&wait.task_id).bind(&wait.turn_id).execute(&mut *tx).await?.rows_affected();
        ensure!(task == 1, "Task no longer authorizes this input wait");
        let condition: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM task_input_conditions WHERE condition_id=? AND task_id=? AND (event_id IS NULL OR consumed_run_id IS NOT ?))")
            .bind(&wait.condition_id).bind(&wait.task_id).bind(&wait.run_id).fetch_one(&mut *tx).await?;
        ensure!(
            condition,
            "Input condition changed before the wait was committed"
        );
        let checkpoint=sqlx::query("UPDATE turn_checkpoints SET status='waiting_input',messages_json=?,usage_json=?,iterations=?,step_seq=?,round_id=?,round_no=?,updated_at=datetime('now') WHERE turn_id=? AND run_id=? AND session_id=? AND status IN ('open','done','waiting_input')")
            .bind(snapshot.messages_json).bind(snapshot.usage_json).bind(snapshot.iterations).bind(snapshot.iterations).bind(snapshot.round_id).bind(snapshot.round_no).bind(&wait.turn_id).bind(&wait.run_id).bind(&wait.session_id).execute(&mut *tx).await?.rows_affected();
        ensure!(
            checkpoint == 1,
            "Input wait has no exact recoverable checkpoint"
        );
        let execution=sqlx::query("UPDATE task_runs SET status='waiting',stop_reason=? WHERE id=? AND task_id=? AND session_id=? AND status='running'")
            .bind(INPUT_WAIT_REASON).bind(&wait.run_id).bind(&wait.task_id).bind(&wait.session_id).execute(&mut *tx).await?.rows_affected();
        ensure!(execution == 1, "Input wait execution is no longer active");
        sqlx::query("INSERT INTO task_input_waits(task_id,condition_id,run_id,turn_id,session_id) VALUES(?,?,?,?,?) ON CONFLICT(task_id) DO UPDATE SET condition_id=excluded.condition_id,run_id=excluded.run_id,turn_id=excluded.turn_id,session_id=excluded.session_id,state='waiting',created_at=datetime('now')")
            .bind(&wait.task_id).bind(&wait.condition_id).bind(&wait.run_id).bind(&wait.turn_id).bind(&wait.session_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn pending(&self) -> Result<Vec<InputWait>> {
        Ok(sqlx::query_as("SELECT task_id,condition_id,run_id,turn_id,session_id,state FROM task_input_waits WHERE state='waiting' ORDER BY created_at")
            .fetch_all(&self.pool).await?)
    }
    pub async fn waiting(&self, task_id: &str) -> Result<Option<InputWait>> {
        Ok(sqlx::query_as("SELECT task_id,condition_id,run_id,turn_id,session_id,state FROM task_input_waits WHERE task_id=?")
            .bind(task_id).fetch_optional(&self.pool).await?)
    }
    pub async fn cancel(&self, task_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE task_input_waits SET state='cancelled' WHERE task_id=? AND state='waiting'",
        )
        .bind(task_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    pub async fn claim(&self, wait: &InputWait, messages_json: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let claimed=sqlx::query("UPDATE task_input_waits SET state='claimed' WHERE task_id=? AND condition_id=? AND run_id=? AND turn_id=? AND state='waiting'
            AND EXISTS(SELECT 1 FROM task_input_conditions c WHERE c.condition_id=task_input_waits.condition_id AND c.event_id IS NOT NULL)
            AND EXISTS(SELECT 1 FROM task_runs r WHERE r.id=task_input_waits.run_id AND r.task_id=task_input_waits.task_id AND r.session_id=task_input_waits.session_id AND r.status='waiting')
            AND NOT EXISTS(SELECT 1 FROM task_pause_fences p WHERE p.task_id=task_input_waits.task_id)
            AND EXISTS(SELECT 1 FROM agent_tasks t WHERE t.id=task_input_waits.task_id AND t.status='suspended' AND t.suspend_reason=? AND t.current_turn_id=task_input_waits.turn_id)")
            .bind(&wait.task_id).bind(&wait.condition_id).bind(&wait.run_id).bind(&wait.turn_id).bind(INPUT_WAIT_REASON).execute(&mut *tx).await?.rows_affected();
        if claimed == 0 {
            tx.rollback().await?;
            return Ok(false);
        }
        let checkpoint=sqlx::query("UPDATE turn_checkpoints SET status='open',messages_json=?,updated_at=datetime('now') WHERE turn_id=? AND run_id=? AND session_id=? AND status='waiting_input'")
            .bind(messages_json).bind(&wait.turn_id).bind(&wait.run_id).bind(&wait.session_id).execute(&mut *tx).await?.rows_affected();
        ensure!(
            checkpoint == 1,
            "Input checkpoint changed before wake admission"
        );
        sqlx::query("UPDATE agent_tasks SET status='running',suspend_reason=NULL,updated_at=datetime('now') WHERE id=?")
            .bind(&wait.task_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE task_input_conditions SET consumed_run_id=? WHERE condition_id=?")
            .bind(&wait.run_id)
            .bind(&wait.condition_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }
}
