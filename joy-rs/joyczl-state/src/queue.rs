//! Messages that run **after** the turn already in flight (0.16 follow-up).
//!
//! `turn/steer` adds to the running turn; this is the other half — "do this next, once you are
//! done with what you are doing". Two things make it a table rather than a queue in the server:
//!
//!   1. **A promise to a person.** Held in process memory, a restart would silently drop an
//!      instruction the human typed and was told was accepted.
//!   2. **One at a time.** The next message is taken only when a turn ends, so the session gate
//!      is never fought over by the queue itself.
//!
//! Claimed rows survive until completion. A restart marks uncertain claims for attention;
//! it never automatically repeats an instruction that may already have caused side effects.

use sqlx::SqlitePool;

/// One queued instruction, as stored.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct QueuedMessage {
    pub id: i64,
    pub session_id: String,
    pub message: String,
    pub created_at: String,
}

/// Queue accessor. Clone is cheap (one pool handle), same reason as `Inbox`.
#[derive(Clone)]
pub struct Queue {
    pool: SqlitePool,
}

impl Queue {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Queue one message and return its position in this session (1 = next up).
    ///
    /// Position is counted for the human ("you are second in line"), so it is read *after* the
    /// insert: the message just queued is included in its own position.
    pub async fn push(&self, session_id: &str, message: &str) -> Result<i64, sqlx::Error> {
        sqlx::query("INSERT INTO session_queue (session_id, message) VALUES (?, ?)")
            .bind(session_id)
            .bind(message)
            .execute(&self.pool)
            .await?;
        self.count(session_id).await
    }

    /// How many messages are waiting in this session.
    pub async fn count(&self, session_id: &str) -> Result<i64, sqlx::Error> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM session_queue WHERE session_id = ? AND status = 'pending'",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Atomically claim the oldest instruction, retaining its identity and contents.
    /// Only one claim per session can be outstanding, including across drain races.
    pub async fn pop(&self, session_id: &str) -> Result<Option<QueuedMessage>, sqlx::Error> {
        sqlx::query_as(
            "UPDATE session_queue SET status = 'claimed' WHERE id = (
                SELECT id FROM session_queue WHERE session_id = ? AND status = 'pending'
                AND NOT EXISTS (SELECT 1 FROM session_queue WHERE session_id = ? AND status = 'claimed')
                ORDER BY id LIMIT 1
             ) RETURNING id, session_id, message, created_at",
        )
        .bind(session_id)
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// Complete after the turn or its failure has been durably recorded.
    pub async fn complete(&self, id: i64) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM session_queue WHERE id = ? AND status = 'claimed'")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Admission was refused before execution: preserve FIFO position for the next drain.
    pub async fn release(&self, id: i64) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE session_queue SET status = 'pending' WHERE id = ? AND status = 'claimed'",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Unknown outcomes remain inspectable and are never automatically replayed.
    pub async fn reconcile_interrupted(&self) -> Result<Vec<QueuedMessage>, sqlx::Error> {
        sqlx::query_as(
            "UPDATE session_queue SET status = 'needs_attention' WHERE status = 'claimed'
             RETURNING id, session_id, message, created_at",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Sessions that still have something queued — the boot drain's work list.
    ///
    /// Ordered by session so a restart behaves the same way twice; order *between* sessions does
    /// not matter, since the session gate serialises each one anyway.
    pub async fn sessions_with_pending(&self) -> Result<Vec<String>, sqlx::Error> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT DISTINCT session_id FROM session_queue WHERE status = 'pending' ORDER BY session_id")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(|row| row.0).collect())
    }
}
