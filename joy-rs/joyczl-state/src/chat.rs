//! The raw conversation log, and the metadata row that names a session (migration 0018).
//!
//! Before 0018 a session was only a tag on chat_log: no name, no working directory, and an
//! empty session did not exist anywhere. Now `sessions` holds exactly those two things a tag
//! cannot carry, while chat_log stays the body of the conversation. A session is created by
//! `session/new` (empty, named) or implicitly by the first message written into it.

use anyhow::Result;
use sqlx::sqlite::SqlitePool;

/// One row of chat_log. The body of session history.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MessageRow {
    /// Row number. **Also the paging cursor** — timestamps lie: one turn's two rows share a second.
    pub id: i64,
    pub role: String,
    pub content: String,
    /// `created_at` as-is: SQLite's `datetime('now')`, UTC, space-separated.
    /// Converting to the protocol's ISO 8601 is the exit side's job (see `iso` in
    /// app-server) — this layer only hands over what the DB stores, untouched.
    pub at: String,
    /// This turn's telemetry JSON, only on assistant rows. Also passed out as-is: what
    /// shape it should be parsed into is the protocol layer's business.
    pub meta: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub id: String,
    /// The name a human gave it, or — when there is none — the first user message.
    pub title: String,
    pub messages: i64,
    pub started_at: Option<String>,
    pub last_at: Option<String>,
    /// The working directory this session works in: the root of what it may write. `None` = not
    /// chosen yet, which means write tools refuse every write (see `joyczl_tools::fs_write`).
    pub cwd: Option<String>,
}

#[derive(Clone)]
pub struct Chat {
    pool: SqlitePool,
}

impl Chat {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Record one exchange. `meta` is the turn's telemetry JSON (gate/graph/iterations/…),
    /// hung only on the assistant row — reopening an old session still shows which brain answered.
    pub async fn append_exchange(
        &self,
        user: &str,
        assistant: &str,
        session_id: &str,
        source: &str,
        meta: Option<&str>,
    ) -> Result<()> {
        self.append_exchange_for(user, assistant, session_id, source, meta, None)
            .await
    }

    /// Same, plus the task this exchange belongs to (migration 0017). A task-less turn passes
    /// `None` — the delegation above keeps every older caller exactly as it was.
    pub async fn append_exchange_for(
        &self,
        user: &str,
        assistant: &str,
        session_id: &str,
        source: &str,
        meta: Option<&str>,
        task_id: Option<&str>,
    ) -> Result<()> {
        self.append_turn_for(user, &[], assistant, session_id, source, meta, task_id)
            .await
    }

    /// One turn written the way it happened: the ask, every **steering** message accepted while
    /// it ran, then the reply.
    ///
    /// Steers get their own rows rather than being glued onto the ask, because the transcript
    /// must show them where they happened. `session_history` folds consecutive user rows back
    /// into one ask, so the model still sees a (question, answer) pair.
    // Eight arguments, all distinct columns of one INSERT — a builder would add a type without
    // removing a question.
    #[allow(clippy::too_many_arguments)]
    pub async fn append_turn_for(
        &self,
        user: &str,
        steers: &[String],
        assistant: &str,
        session_id: &str,
        source: &str,
        meta: Option<&str>,
        task_id: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO chat_log (role, content, session_id, source, task_id)
             VALUES ('user', ?, ?, ?, ?)",
        )
        .bind(user)
        .bind(session_id)
        .bind(source)
        .bind(task_id)
        .execute(&self.pool)
        .await?;
        for steer in steers {
            sqlx::query(
                "INSERT INTO chat_log (role, content, session_id, source, meta, task_id)
                 VALUES ('user', ?, ?, ?, '{\"steer\":true}', ?)",
            )
            .bind(steer)
            .bind(session_id)
            .bind(source)
            .bind(task_id)
            .execute(&self.pool)
            .await?;
        }
        sqlx::query(
            "INSERT INTO chat_log (role, content, session_id, source, meta, task_id)
             VALUES ('assistant', ?, ?, ?, ?, ?)",
        )
        .bind(assistant)
        .bind(session_id)
        .bind(source)
        .bind(meta)
        .bind(task_id)
        .execute(&self.pool)
        .await?;
        // A session exists as soon as something is said in it — REPL, gateway and schedule turns
        // never call `session/new`, and they must not produce a conversation nobody can list.
        self.touch_session(session_id).await?;
        Ok(())
    }

    /// Make sure the session row exists and mark it active now. One statement: the list sorts by
    /// `last_at`, and two conversations from the same second must still be ordered by real use.
    pub async fn touch_session(&self, session_id: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO sessions (id, last_at) VALUES (?, datetime('now'))
             ON CONFLICT(id) DO UPDATE SET last_at = excluded.last_at",
        )
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Rows pending distillation (in time order). Consolidation's input and threshold check.
    pub async fn unconsolidated(&self) -> Result<Vec<(i64, String, String)>> {
        let rows = sqlx::query_as::<_, (i64, String, String)>(
            // Only rows that are **due**: a failed batch carries a future `consolidation_next_at`
            // and must not be re-fetched early (else every turn burns a model call for nothing).
            "SELECT id, role, content FROM chat_log
             WHERE consolidated = 0
               AND (consolidation_next_at IS NULL OR consolidation_next_at <= datetime('now'))
             ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// **Claim** a batch of pending rows: one SQL does "select + mark", returning the claimed batch.
    ///
    /// Why not "read a batch → distill → then mark": in that order, two concurrent turns
    /// (a REPL turn + a dashboard turn, or schedule and user firing together) both see
    /// "due" **at the same time**, and the same logs get distilled twice — a wasted model
    /// call and an extra `consolidation_tries` (dedup at the write entry is a slower gate).
    ///
    /// Claiming is atomic: the loser gets an empty list and **never calls the model**. The
    /// cost is "mark first, process after", so every failure path must
    /// `mark_consolidation_failed` to hand the claim back.
    pub async fn claim_unconsolidated(&self) -> Result<Vec<(i64, String, String)>> {
        // Same WHERE as `unconsolidated` (only due rows), just SELECT swapped for
        // UPDATE … RETURNING: no gap between marking and taking.
        let rows = sqlx::query_as::<_, (i64, String, String)>(
            "UPDATE chat_log SET consolidated = 1
             WHERE id IN (
                 SELECT id FROM chat_log
                 WHERE consolidated = 0
                   AND (consolidation_next_at IS NULL OR consolidation_next_at <= datetime('now'))
                 ORDER BY id
             )
             RETURNING id, role, content",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Distillation failed: **hand the claim back** and schedule into the future with exponential backoff.
    ///
    /// Claiming marks first and processes after, so on failure `consolidated` must be reset
    /// to zero — otherwise the logs are marked "distilled" while nothing was written: real
    /// data loss. 1 min start, doubling, 1 hour cap: a never-distillable record must not be
    /// retried every turn, nor abandoned (the model may just be flaky right now).
    pub async fn mark_consolidation_failed(&self, ids: &[i64]) -> Result<()> {
        for id in ids {
            let tries: i64 =
                sqlx::query_scalar("SELECT consolidation_tries FROM chat_log WHERE id = ?")
                    .bind(id)
                    .fetch_optional(&self.pool)
                    .await?
                    .unwrap_or(0);
            let backoff = (60i64 * 2i64.pow(tries.min(6) as u32)).min(3_600);
            sqlx::query(
                "UPDATE chat_log
                 SET consolidation_tries = consolidation_tries + 1,
                     consolidation_next_at = datetime('now', ?),
                     consolidated = 0
                 WHERE id = ?",
            )
            .bind(format!("+{backoff} seconds"))
            .bind(id)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    // ("Mark as consolidated" is no longer a separate step: `claim_unconsolidated` marked
    //  them at claim time. The old `mark_consolidated` was deleted with it — keeping it
    //  would leave a door open for "mark it again after it's already marked".)

    /// An old session's (user, assistant) turns in order — rebuilds working memory on switch-back.
    pub async fn session_history(&self, session_id: &str) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT role, content FROM chat_log WHERE session_id = ? ORDER BY id",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;

        let mut pairs = Vec::new();
        // Several user rows in a row are **one ask** — a turn's original message plus whatever was
        // steered into it while it ran. Joined in order, the model sees the same single question
        // it saw live; kept as "last one wins", the original ask would silently vanish.
        let mut pending: Vec<String> = Vec::new();
        for (role, content) in rows {
            match role.as_str() {
                "user" => pending.push(content),
                "assistant" if !pending.is_empty() => {
                    pairs.push((pending.join("\n\n"), content));
                    pending.clear();
                }
                _ => {}
            }
        }
        Ok(pairs)
    }

    /// Record one "measured × estimated" pair (calibrating local estimates with provider usage).
    ///
    /// When neither number is positive, record nothing — the graph path (not going through
    /// loop) is exactly that; a ratio computed from 0 would be an absurd multiple.
    pub async fn observe_context(
        &self,
        session_id: &str,
        observed: i64,
        estimated: i64,
    ) -> Result<()> {
        if observed <= 0 || estimated <= 0 {
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO session_context (session_id, observed_input_tokens, estimated_input_tokens, updated_at)
             VALUES (?, ?, ?, datetime('now'))
             ON CONFLICT(session_id) DO UPDATE SET
                observed_input_tokens = excluded.observed_input_tokens,
                estimated_input_tokens = excluded.estimated_input_tokens,
                updated_at = excluded.updated_at",
        )
        .bind(session_id)
        .bind(observed)
        .bind(estimated)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The latest measured/estimated ratio, clamped to `0.5..=2.0`.
    ///
    /// Clamping is deliberate: one anomalous request (say, a 1-token probe) can push the
    /// ratio sky-high — a budget skewed by that is worse than no calibration. No record → `None`.
    pub async fn context_factor(&self, session_id: &str) -> Result<Option<f64>> {
        let row = sqlx::query_as::<_, (i64, i64)>(
            "SELECT observed_input_tokens, estimated_input_tokens
             FROM session_context WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(observed, estimated)| {
            if observed <= 0 || estimated <= 0 {
                return None;
            }
            Some((observed as f64 / estimated as f64).clamp(0.5, 2.0))
        }))
    }

    /// This session's rolling summary: (how many turns covered, the summary).
    ///
    /// No summary, or an unreadable row, both count as "none yet" — the summary is only a
    /// token-saving device; broken, it at most pads the prompt with raw turns.
    pub async fn load_rollup(&self, session_id: &str) -> Result<Option<(i32, String)>> {
        let row = sqlx::query_as::<_, (i64, String)>(
            "SELECT covered_turns, summary FROM context_rollups WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(covered, summary)| (covered as i32, summary)))
    }

    /// Overwrite the rolling summary. One row per session, **only rolled forward**.
    ///
    /// Returns "did this write land". `false` = blocked by the monotonic guard: the DB
    /// already holds a summary with a larger (i.e. newer) `covered_turns`; this one is dropped.
    ///
    /// Why the guard: compaction can happen in two concurrent turns at once (no
    /// serialization per session, see docs/limitations.md), and the later writer doesn't
    /// necessarily win — without it, users would get an **older** summary, no anomaly shown.
    pub async fn save_rollup(
        &self,
        session_id: &str,
        covered_turns: i32,
        summary: &str,
    ) -> Result<bool> {
        let result = sqlx::query(
            // `WHERE excluded.covered_turns > context_rollups.covered_turns`: accept only
            // newer ones. When that condition fails on the conflict row, DO UPDATE does
            // nothing (SQLite's defined semantics), so rows_affected() = 0.
            "INSERT INTO context_rollups (session_id, covered_turns, summary, updated_at)
             VALUES (?, ?, ?, datetime('now'))
             ON CONFLICT(session_id) DO UPDATE SET
                covered_turns = excluded.covered_turns,
                summary = excluded.summary,
                updated_at = excluded.updated_at
             WHERE excluded.covered_turns > context_rollups.covered_turns",
        )
        .bind(session_id)
        .bind(covered_turns as i64)
        .bind(summary)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// One session's messages, **newest first**.
    ///
    /// The direction is the reverse of the conversation itself, on purpose: this method
    /// either fetches "the most recent page" or "the page earlier than some id" — both
    /// walk from new to old; arranging it for humans on screen is the display layer's job.
    ///
    /// `before` means "only rows earlier than this id"; absent = the newest page.
    /// Defaulting to `i64::MAX` instead of two SQL strings: `id < ?` is one expression,
    /// avoiding the `(? IS NULL OR ...)` shape that binds twice.
    pub async fn messages(
        &self,
        session_id: &str,
        before: Option<i64>,
        limit: u32,
    ) -> Result<Vec<MessageRow>> {
        sqlx::query_as::<_, MessageRow>(
            "SELECT id, role, content, created_at AS at, meta
               FROM chat_log
              WHERE session_id = ? AND id < ?
              ORDER BY id DESC
              LIMIT ?",
        )
        .bind(session_id)
        .bind(before.unwrap_or(i64::MAX))
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// All sessions, the most recently active first. Empty ones (created but never spoken in)
    /// belong in the list exactly like the rest — that is what the table is for.
    pub async fn sessions(&self) -> Result<Vec<SessionRow>> {
        let rows = sqlx::query_as::<_, RawSession>(&format!(
            "{SESSION_COLUMNS} ORDER BY COALESCE(s.last_at, s.created_at) DESC"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(SessionRow::from).collect())
    }

    /// One session's metadata. `None` = no such session (never created, or the id is a typo).
    pub async fn session(&self, id: &str) -> Result<Option<SessionRow>> {
        let row = sqlx::query_as::<_, RawSession>(&format!("{SESSION_COLUMNS} WHERE s.id = ?"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(SessionRow::from))
    }

    /// Create a session row. Idempotent: an existing row is **kept**, and only its empty fields
    /// are filled in — `session/new` called twice with the same id must not rename anything.
    /// `last_at` is set so a brand-new empty session sorts as "just now".
    pub async fn create_session(
        &self,
        id: &str,
        title: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO sessions (id, title, cwd, last_at) VALUES (?, ?, ?, datetime('now'))
             ON CONFLICT(id) DO UPDATE SET
                 title = COALESCE(NULLIF(sessions.title, ''), excluded.title),
                 cwd   = COALESCE(sessions.cwd, excluded.cwd)",
        )
        .bind(id)
        .bind(blank_to_null(title))
        .bind(blank_to_null(cwd))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Rename / rebind the working directory. `None` = leave that field alone; `Some("")`
    /// = clear it (title then falls back to the first user message, cwd back to "not chosen").
    pub async fn update_session(
        &self,
        id: &str,
        title: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<()> {
        let mut sets: Vec<&str> = Vec::new();
        if title.is_some() {
            sets.push("title = ?");
        }
        if cwd.is_some() {
            sets.push("cwd = ?");
        }
        if sets.is_empty() {
            return Ok(());
        }
        let sql = format!("UPDATE sessions SET {} WHERE id = ?", sets.join(", "));
        let mut query = sqlx::query(&sql);
        if let Some(t) = title {
            query = query.bind(blank_to_null(Some(t)));
        }
        if let Some(c) = cwd {
            query = query.bind(blank_to_null(Some(c)));
        }
        query.bind(id).execute(&self.pool).await?;
        Ok(())
    }
}

/// One expression for a session's readable fields, shared by the list and the single read, so
/// "the title is the first user message" cannot drift into two different implementations.
const SESSION_COLUMNS: &str = "
    SELECT s.id AS id,
           COALESCE(NULLIF(s.title, ''), (
               SELECT c.content FROM chat_log c
                WHERE c.session_id = s.id AND c.role = 'user'
                ORDER BY c.id LIMIT 1)) AS title,
           (SELECT COUNT(*) FROM chat_log c WHERE c.session_id = s.id) AS messages,
           (SELECT MIN(c.created_at) FROM chat_log c WHERE c.session_id = s.id) AS started_at,
           s.last_at AS last_at,
           s.cwd AS cwd
      FROM sessions s";

#[derive(sqlx::FromRow)]
struct RawSession {
    id: String,
    title: Option<String>,
    messages: i64,
    started_at: Option<String>,
    last_at: Option<String>,
    cwd: Option<String>,
}

impl From<RawSession> for SessionRow {
    fn from(row: RawSession) -> Self {
        SessionRow {
            id: row.id,
            title: row.title.unwrap_or_else(|| "(空会话)".to_string()),
            messages: row.messages,
            started_at: row.started_at,
            last_at: row.last_at,
            cwd: row.cwd,
        }
    }
}

/// Empty / whitespace-only text means "no value": the DB stores NULL, never `""` — otherwise
/// "cleared" and "set to empty" become two states that render differently but mean the same.
fn blank_to_null(text: Option<&str>) -> Option<&str> {
    text.map(str::trim).filter(|t| !t.is_empty())
}
