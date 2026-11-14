//! Schedule state (migration 0012): `joy schedule`'s dedup, crash recovery and per-job
//! delivery policy, all in one state.db table — the old `schedules-state.json` becomes
//! only a one-time import source from now on, no longer authoritative.
//!
//! The model (dispatch advances next_run first against replay; joy version):
//!
//! ```text
//! tick finds it due ──► claim(name, minute)      ← claim the slot first (persisted pre-crash)
//!                    ──► fire() runs it
//!                    ──► confirm(name, minute) ← settled: last_fired = minute
//!
//! On restart, an unsettled claim requires human review; it is never overwritten or replayed.
//! Catch-up only applies to missed, unclaimed trigger times.
//! ```

use sqlx::SqlitePool;

/// One schedule-state row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ScheduleRow {
    pub name: String,
    pub last_fired_minute: Option<String>,
    pub claimed_minute: Option<String>,
    pub catch_up: bool,
    pub delivery: String,
}

/// Schedule state accessor. Clone is cheap (one pool handle), same reason as Jobs.
#[derive(Clone)]
pub struct Schedules {
    pool: SqlitePool,
}

impl Schedules {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Register a declared task: create if absent, else refresh policy fields per the
    /// current declaration (catch_up/delivery follow the **current declaration**); history kept.
    pub async fn ensure(
        &self,
        name: &str,
        catch_up: bool,
        delivery: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO schedule_state (name, catch_up, delivery) VALUES (?, ?, ?) \
             ON CONFLICT(name) DO UPDATE SET catch_up = excluded.catch_up, \
             delivery = excluded.delivery, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')",
        )
        .bind(name)
        .bind(catch_up)
        .bind(delivery)
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    /// The last settled trigger minute (for dedup).
    pub async fn last_fired(&self, name: &str) -> Result<Option<String>, sqlx::Error> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT last_fired_minute FROM schedule_state WHERE name = ?")
                .bind(name)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(m,)| m))
    }

    /// The unconfirmed claim (for restart recovery). A value = the process died mid-run.
    pub async fn pending_claim(&self, name: &str) -> Result<Option<String>, sqlx::Error> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT claimed_minute FROM schedule_state WHERE name = ?")
                .bind(name)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(m,)| m))
    }

    /// Claim: persist "I'm running this minute" before dispatch. A crash here leaves
    /// pending_claim non-empty — the replay is explicitly taken over, not left to luck.
    pub async fn claim(&self, name: &str, minute: &str) -> Result<(), sqlx::Error> {
        if self.claim_once(name, minute).await? {
            Ok(())
        } else {
            Err(sqlx::Error::Protocol(
                "Schedule already claimed or handled; review before retrying".into(),
            ))
        }
    }
    pub async fn claim_once(&self, name: &str, minute: &str) -> Result<bool, sqlx::Error> {
        let changed=sqlx::query("UPDATE schedule_state SET claimed_minute=?,updated_at=strftime('%Y-%m-%dT%H:%M:%SZ','now')
            WHERE name=? AND claimed_minute IS NULL AND (last_fired_minute IS NULL OR last_fired_minute<>?)
            AND NOT EXISTS(SELECT 1 FROM schedule_completions c WHERE c.name=schedule_state.name AND c.minute=?)")
            .bind(minute).bind(name).bind(minute).bind(minute).execute(&self.pool).await?.rows_affected();
        Ok(changed == 1)
    }

    /// Legacy administrative settlement. Executions use ScheduleCompletions::settle so the
    /// outcome and production receipt commit with the trigger state.
    pub async fn confirm(&self, name: &str, minute: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE schedule_state SET last_fired_minute = ?, claimed_minute = NULL, \
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE name = ?",
        )
        .bind(minute)
        .bind(name)
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    /// A snapshot of all rows (the dashboard's schedule panel). Sorted by name; the caller handles read failures.
    pub async fn list(&self) -> Result<Vec<ScheduleRow>, sqlx::Error> {
        sqlx::query_as(
            "SELECT name, last_fired_minute, claimed_minute, catch_up, delivery              FROM schedule_state ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// One-time import from the old JSON: only last_fired moves, policy fields untouched. Idempotent —
    /// never overwrites existing DB values (the scheduler only calls it when last_fired is empty).
    pub async fn import_legacy(&self, name: &str, minute: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE schedule_state SET last_fired_minute = ?, \
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE name = ?",
        )
        .bind(minute)
        .bind(name)
        .execute(&self.pool)
        .await
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> (Schedules, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let pool = crate::open(&dir.path().join("state.db"))
            .await
            .expect("open the store");
        (Schedules::new(pool), dir)
    }

    /// ensure creates rows, refreshes policy but keeps history; the full claim → pending → confirm chain.
    #[tokio::test]
    async fn ensure_claim_confirm_round_trip() {
        let (schedules, _dir) = db().await;
        assert!(schedules
            .last_fired("morning")
            .await
            .expect("empty")
            .is_none());

        schedules
            .ensure("morning", true, "outbox")
            .await
            .expect("record");
        schedules
            .claim("morning", "2026-10-30T08:00")
            .await
            .expect("claim the slot");
        assert_eq!(
            schedules
                .pending_claim("morning")
                .await
                .expect("read the slot")
                .as_deref(),
            Some("2026-10-30T08:00")
        );
        schedules
            .confirm("morning", "2026-10-30T08:00")
            .await
            .expect("settle");
        assert!(schedules
            .pending_claim("morning")
            .await
            .expect("the slot is clear")
            .is_none());
        assert_eq!(
            schedules
                .last_fired("morning")
                .await
                .expect("read back")
                .as_deref(),
            Some("2026-10-30T08:00")
        );

        // ensure again: policy refreshed, history kept.
        schedules
            .ensure("morning", false, "notify")
            .await
            .expect("record again");
        let raw: (bool, String, Option<String>) = sqlx::query_as(
            "SELECT catch_up, delivery, last_fired_minute FROM schedule_state WHERE name = 'morning'",
        )
        .fetch_one(&schedules.pool)
        .await
        .expect("read the row");
        assert!(!raw.0, "policy follows the latest declaration");
        assert_eq!(raw.1, "notify");
        assert_eq!(
            raw.2.as_deref(),
            Some("2026-10-30T08:00"),
            "the history is not lost"
        );
    }

    /// Administrative confirmation can abandon a slot after human review without replay.
    #[tokio::test]
    async fn confirm_doubles_as_the_abandon_exit() {
        let (schedules, _dir) = db().await;
        schedules
            .ensure("notify-job", false, "notify")
            .await
            .expect("record");
        schedules
            .claim("notify-job", "2026-10-30T09:00")
            .await
            .expect("claim the slot");
        // The operator explicitly discards a reviewed slot; no work is rerun.
        schedules
            .confirm("notify-job", "2026-10-30T09:00")
            .await
            .expect("give up");
        assert!(schedules
            .pending_claim("notify-job")
            .await
            .expect("read")
            .is_none());
        assert_eq!(
            schedules
                .last_fired("notify-job")
                .await
                .expect("read")
                .as_deref(),
            Some("2026-10-30T09:00")
        );
    }

    /// list reads back every row, sorted by name.
    #[tokio::test]
    async fn list_returns_every_row_sorted() {
        let (schedules, _dir) = db().await;
        for (name, cu, dl) in [("b", true, "outbox"), ("a", false, "notify")] {
            schedules.ensure(name, cu, dl).await.expect("record");
        }
        let rows = schedules.list().await.expect("read back");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "a");
        assert!(!rows[0].catch_up);
        assert_eq!(rows[1].delivery, "outbox");
    }

    /// import_legacy only moves history; when the DB already has a value (the caller
    /// promises not to call, but semantically) it overwrites — so the scheduler checks first.
    #[tokio::test]
    async fn legacy_import_moves_only_history() {
        let (schedules, _dir) = db().await;
        schedules
            .ensure("old", true, "outbox")
            .await
            .expect("record");
        schedules
            .import_legacy("old", "2026-10-29T08:00")
            .await
            .expect("import");
        assert_eq!(
            schedules.last_fired("old").await.expect("read").as_deref(),
            Some("2026-10-29T08:00")
        );
    }
    #[tokio::test]
    async fn an_unsettled_claim_cannot_be_overwritten_by_another_trigger() {
        let (schedules, _dir) = db().await;
        schedules.ensure("job", true, "outbox").await.unwrap();
        schedules.claim("job", "2026-11-13T08:00").await.unwrap();
        assert!(schedules.claim("job", "2026-11-13T08:01").await.is_err());
        assert_eq!(
            schedules.pending_claim("job").await.unwrap().as_deref(),
            Some("2026-11-13T08:00")
        );
    }
}
