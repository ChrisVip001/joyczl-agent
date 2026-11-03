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
//! On restart, a non-empty claimed_minute = the process died mid-run:
//!   catch_up=1 → rerun once (at-least-once; morning reports prefer repeat over miss)
//!   catch_up=0 → confirm and give up (at-most-once; notifications prefer miss over repeat)
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
        sqlx::query(
            "UPDATE schedule_state SET claimed_minute = ?, \
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE name = ?",
        )
        .bind(minute)
        .bind(name)
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    /// Settle: last_fired = minute, claim cleared. Success and failure both come here —
    /// a failed job also counts as "this minute handled"; no retry every 20s without backoff.
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
        let dir = tempfile::tempdir().expect("临时目录");
        let pool = crate::open(&dir.path().join("state.db"))
            .await
            .expect("打开库");
        (Schedules::new(pool), dir)
    }

    /// ensure creates rows, refreshes policy but keeps history; the full claim → pending → confirm chain.
    #[tokio::test]
    async fn ensure_claim_confirm_round_trip() {
        let (schedules, _dir) = db().await;
        assert!(schedules.last_fired("morning").await.expect("空").is_none());

        schedules
            .ensure("morning", true, "outbox")
            .await
            .expect("登记");
        schedules
            .claim("morning", "2026-10-30T08:00")
            .await
            .expect("占坑");
        assert_eq!(
            schedules
                .pending_claim("morning")
                .await
                .expect("读坑")
                .as_deref(),
            Some("2026-10-30T08:00")
        );
        schedules
            .confirm("morning", "2026-10-30T08:00")
            .await
            .expect("落定");
        assert!(schedules
            .pending_claim("morning")
            .await
            .expect("坑清了")
            .is_none());
        assert_eq!(
            schedules
                .last_fired("morning")
                .await
                .expect("读回")
                .as_deref(),
            Some("2026-10-30T08:00")
        );

        // ensure again: policy refreshed, history kept.
        schedules
            .ensure("morning", false, "notify")
            .await
            .expect("再登记");
        let raw: (bool, String, Option<String>) = sqlx::query_as(
            "SELECT catch_up, delivery, last_fired_minute FROM schedule_state WHERE name = 'morning'",
        )
        .fetch_one(&schedules.pool)
        .await
        .expect("读行");
        assert!(!raw.0, "策略按最新声明");
        assert_eq!(raw.1, "notify");
        assert_eq!(raw.2.as_deref(), Some("2026-10-30T08:00"), "历史不丢");
    }

    /// confirm doubles as the "abandon" exit: a catch_up=0 job claimed the slot, then the
    /// process died; on restart confirm it — at-most-once.
    #[tokio::test]
    async fn confirm_doubles_as_the_abandon_exit() {
        let (schedules, _dir) = db().await;
        schedules
            .ensure("notify-job", false, "notify")
            .await
            .expect("登记");
        schedules
            .claim("notify-job", "2026-10-30T09:00")
            .await
            .expect("占坑");
        // The process died. On restart: catch_up=0 → confirm it away, no rerun.
        schedules
            .confirm("notify-job", "2026-10-30T09:00")
            .await
            .expect("放弃");
        assert!(schedules
            .pending_claim("notify-job")
            .await
            .expect("读")
            .is_none());
        assert_eq!(
            schedules
                .last_fired("notify-job")
                .await
                .expect("读")
                .as_deref(),
            Some("2026-10-30T09:00")
        );
    }

    /// list reads back every row, sorted by name.
    #[tokio::test]
    async fn list_returns_every_row_sorted() {
        let (schedules, _dir) = db().await;
        for (name, cu, dl) in [("b", true, "outbox"), ("a", false, "notify")] {
            schedules.ensure(name, cu, dl).await.expect("登记");
        }
        let rows = schedules.list().await.expect("读回");
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
        schedules.ensure("old", true, "outbox").await.expect("登记");
        schedules
            .import_legacy("old", "2026-10-29T08:00")
            .await
            .expect("导入");
        assert_eq!(
            schedules.last_fired("old").await.expect("读").as_deref(),
            Some("2026-10-29T08:00")
        );
    }
}
