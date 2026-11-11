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
//! There is no status column: a row exists exactly while it has not been handed to a turn
//! (claim-then-run, see [`Queue::pop`]). Once it runs, chat_log is the record.

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
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM session_queue WHERE session_id = ?")
            .bind(session_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0)
    }

    /// Take the oldest message for a session, **removing it**.
    ///
    /// Claim-then-run on purpose. Keeping the row until the turn succeeds would replay an
    /// instruction whose outcome is unknown after a crash — exactly the failure mode 0013/0017
    /// exist to avoid ("a step left running is never replayed"). The cost is the opposite
    /// failure: a crash between this delete and the turn's first write loses that one
    /// instruction. It is documented, and it is the cheaper of the two.
    pub async fn pop(&self, session_id: &str) -> Result<Option<QueuedMessage>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let row: Option<QueuedMessage> = sqlx::query_as(
            "SELECT id, session_id, message, created_at FROM session_queue
             WHERE session_id = ? ORDER BY id LIMIT 1",
        )
        .bind(session_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = &row {
            sqlx::query("DELETE FROM session_queue WHERE id = ?")
                .bind(row.id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(row)
    }

    /// Sessions that still have something queued — the boot drain's work list.
    ///
    /// Ordered by session so a restart behaves the same way twice; order *between* sessions does
    /// not matter, since the session gate serialises each one anyway.
    pub async fn sessions_with_pending(&self) -> Result<Vec<String>, sqlx::Error> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT DISTINCT session_id FROM session_queue ORDER BY session_id")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(|row| row.0).collect())
    }
}
