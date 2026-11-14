//! The connector inbox (migration 0015): what came in from the outside, recorded as-is.
//!
//! Deliberately dumb: insert, list, mark. Nothing here talks to the network, nothing
//! decides what to do with a message — a person does (or `inbound.mode = "turn"`, which
//! is that person's standing decision).
//!
//! Dedup key is `(connector, mailbox, uid)`: IMAP UIDs are monotonic within a mailbox, so
//! re-polling after a restart inserts nothing twice. A rebuilt mailbox restarts its UIDs;
//! old rows then shadow same-numbered new mail — judged by uid, not message_id (see docs).

use sqlx::SqlitePool;

/// One received message, as stored.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct InboxItem {
    pub id: i64,
    pub connector: String,
    pub mailbox: String,
    pub uid: i64,
    pub message_id: Option<String>,
    pub from_addr: String,
    pub subject: String,
    pub received_at: String,
    pub snippet: String,
    pub body: String,
    pub seen: bool,
}

/// Inbox accessor. Clone is cheap (one pool handle), same reason as Jobs.
#[derive(Clone)]
pub struct Inbox {
    pool: SqlitePool,
}

impl Inbox {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Record one message. A duplicate (same connector/mailbox/uid) inserts nothing and
    /// returns `false` — the poller counts "new" by this, not by what it fetched.
    #[allow(clippy::too_many_arguments)]
    pub async fn record(
        &self,
        connector: &str,
        mailbox: &str,
        uid: i64,
        message_id: Option<&str>,
        from_addr: &str,
        subject: &str,
        received_at: &str,
        body: &str,
    ) -> Result<bool, sqlx::Error> {
        // The snippet is derived here, once: every reader wants "the first line or two".
        let snippet: String = body
            .chars()
            .take(240)
            .collect::<String>()
            .trim()
            .to_string();
        let result = sqlx::query(
            "INSERT OR IGNORE INTO connector_inbox
             (connector, mailbox, uid, message_id, from_addr, subject, received_at, snippet, body)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(connector)
        .bind(mailbox)
        .bind(uid)
        .bind(message_id)
        .bind(from_addr)
        .bind(subject)
        .bind(received_at)
        .bind(snippet)
        .bind(body)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// The newest messages first — the panel's list, unseen or not.
    pub async fn recent(&self, connector: &str, limit: i64) -> Result<Vec<InboxItem>, sqlx::Error> {
        sqlx::query_as(
            "SELECT * FROM connector_inbox WHERE connector = ?
             ORDER BY received_at DESC, id DESC LIMIT ?",
        )
        .bind(connector)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    /// The ones still waiting for a person (or the standing `turn` policy).
    pub async fn unseen(&self, connector: &str, limit: i64) -> Result<Vec<InboxItem>, sqlx::Error> {
        sqlx::query_as(
            "SELECT * FROM connector_inbox WHERE connector = ? AND seen = 0
             ORDER BY received_at ASC, id ASC LIMIT ?",
        )
        .bind(connector)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    pub async fn unseen_count(&self, connector: &str) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar("SELECT COUNT(*) FROM connector_inbox WHERE connector = ? AND seen = 0")
            .bind(connector)
            .fetch_one(&self.pool)
            .await
    }

    /// The poller's "fetch after this" cursor. `None` = never polled: the first poll only
    /// records the mailbox's high-water mark instead of dumping its history.
    pub async fn cursor(&self, connector: &str, mailbox: &str) -> Result<Option<i64>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT last_uid FROM connector_cursors WHERE connector = ? AND mailbox = ?",
        )
        .bind(connector)
        .bind(mailbox)
        .fetch_optional(&self.pool)
        .await
    }

    /// Advance the cursor. Never moves backwards — a re-poll of an overlapping range must
    /// not rewind what was already seen.
    pub async fn set_cursor(
        &self,
        connector: &str,
        mailbox: &str,
        last_uid: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO connector_cursors (connector, mailbox, last_uid) VALUES (?, ?, ?)
             ON CONFLICT(connector, mailbox) DO UPDATE SET
               last_uid = MAX(connector_cursors.last_uid, excluded.last_uid),
               updated_at = datetime('now')",
        )
        .bind(connector)
        .bind(mailbox)
        .bind(last_uid)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark a batch as handled (a person clicked, or the standing `turn` policy ran).
    /// Ids missing from the table are ignored.
    pub async fn mark_handled(&self, ids: &[i64]) -> Result<(), sqlx::Error> {
        for id in ids {
            sqlx::query("UPDATE connector_inbox SET seen = 1 WHERE id = ?")
                .bind(id)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open;

    async fn store() -> Inbox {
        let dir = tempfile::tempdir().expect("temp dir");
        let pool = open(&dir.path().join("state.db"))
            .await
            .expect("open the store");
        let _ = dir.keep();
        Inbox::new(pool)
    }

    #[tokio::test]
    async fn the_same_uid_twice_is_recorded_once() {
        let inbox = store().await;
        let first = inbox
            .record(
                "email",
                "INBOX",
                42,
                Some("<m1@x>"),
                "a@corp.com",
                "weekly report",
                "2026-11-05T08:00:00Z",
                "body text",
            )
            .await
            .expect("write");
        let second = inbox
            .record(
                "email",
                "INBOX",
                42,
                Some("<m1@x>"),
                "a@corp.com",
                "weekly report",
                "2026-11-05T08:00:00Z",
                "body text",
            )
            .await
            .expect("write again");
        assert!(first, "the first time is new mail");
        assert!(!second, "the same UID must not count as new again");
        assert_eq!(inbox.recent("email", 10).await.expect("read").len(), 1);
    }

    #[tokio::test]
    async fn unseen_counts_and_the_uid_cursor_move_together() {
        let inbox = store().await;
        for uid in [7, 9, 12] {
            inbox
                .record(
                    "email",
                    "INBOX",
                    uid,
                    None,
                    "a@corp.com",
                    "s",
                    "2026-11-05T08:00:00Z",
                    "b",
                )
                .await
                .expect("write");
        }
        assert_eq!(inbox.unseen_count("email").await.expect("count"), 3);

        assert_eq!(inbox.cursor("email", "INBOX").await.expect("cursor"), None);
        inbox
            .set_cursor("email", "INBOX", 12)
            .await
            .expect("write the cursor");
        inbox
            .set_cursor("email", "INBOX", 9)
            .await
            .expect("going backwards must not take effect");
        assert_eq!(
            inbox.cursor("email", "INBOX").await.expect("cursor"),
            Some(12)
        );

        let waiting = inbox.unseen("email", 10).await.expect("read");
        let ids: Vec<i64> = waiting.iter().map(|item| item.id).collect();
        inbox.mark_handled(&ids).await.expect("mark");

        assert_eq!(inbox.unseen_count("email").await.expect("count"), 0);
        let recent = inbox.recent("email", 10).await.expect("read");
        assert!(recent.iter().all(|item| item.seen));
        // The cursor is about uids, not about being seen.
        assert_eq!(
            inbox.cursor("email", "INBOX").await.expect("cursor"),
            Some(12)
        );
    }

    #[tokio::test]
    async fn connectors_do_not_see_each_others_mail() {
        let inbox = store().await;
        inbox
            .record(
                "email",
                "INBOX",
                1,
                None,
                "a@corp.com",
                "s",
                "2026-11-05T08:00:00Z",
                "b",
            )
            .await
            .expect("write");
        assert!(inbox.recent("feishu", 10).await.expect("read").is_empty());
        assert_eq!(inbox.unseen_count("feishu").await.expect("count"), 0);
    }

    #[tokio::test]
    async fn the_snippet_is_derived_from_the_body_once() {
        let inbox = store().await;
        inbox
            .record(
                "email",
                "INBOX",
                3,
                None,
                "a@corp.com",
                "s",
                "2026-11-05T08:00:00Z",
                &"x".repeat(400),
            )
            .await
            .expect("write");
        let item = &inbox.recent("email", 1).await.expect("read")[0];
        assert_eq!(
            item.snippet.chars().count(),
            240,
            "the snippet is cut by characters"
        );
        assert_eq!(item.body.chars().count(), 400, "the body is kept whole");
    }
}
