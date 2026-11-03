//! The local calendar: events Joy created itself.
//!
//! Semantics:
//!   * state.db is always the authoritative landing spot (deterministic evaluation asserts here);
//!   * `calendar.ics` is an importable file copy;
//!   * **Idempotent**: an event with the same title + same start time is stored once — a
//!     confused model or an impatient user must not cause triple booking.
//!
//! Apple / Google calendar sync belongs to the tool layer (joyczl-tools): state only
//! manages this local table, and a failed online sync must not affect local writes.

use anyhow::Result;
use sqlx::sqlite::SqlitePool;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct CalendarRow {
    pub id: i64,
    pub title: String,
    /// ISO 8601, minute precision.
    pub start: String,
    /// Reserved word `"end"` in SQL; the SELECT column name is `end`, so is the field.
    pub end: String,
    pub attendees: String,
    pub notes: String,
    pub created_at: Option<String>,
}

#[derive(Clone)]
pub struct Calendar {
    pool: SqlitePool,
}

impl Calendar {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Write one event. If the same (title, start) exists, no duplicate write; returns `Ok(None)` —
    /// the caller then tells the model "the event is already there" instead of faking a new one.
    pub async fn add(
        &self,
        title: &str,
        start: &str,
        end: &str,
        attendees: &str,
        notes: &str,
    ) -> Result<Option<CalendarRow>> {
        let row = sqlx::query_as::<_, CalendarRow>(
            r#"INSERT OR IGNORE INTO calendar_events (title, start, "end", attendees, notes)
               VALUES (?, ?, ?, ?, ?)
               RETURNING id, title, start, "end", attendees, notes, created_at"#,
        )
        .bind(title)
        .bind(start)
        .bind(end)
        .bind(attendees)
        .bind(notes)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Search by a start-date range (both ends inclusive; `start`/`end` are ISO dates).
    /// Both absent = everything, ordered by start time ascending.
    pub async fn list(
        &self,
        start: Option<&str>,
        end: Option<&str>,
        limit: u32,
    ) -> Result<Vec<CalendarRow>> {
        let mut builder = sqlx::QueryBuilder::new(
            r#"SELECT id, title, start, "end", attendees, notes, created_at
               FROM calendar_events"#,
        );
        let mut has_where = false;
        if let Some(start) = start {
            // Compare only the date part (first 10 chars): with ISO, equal prefixes mean the
            // same day — "July 14 plans" won't miss the 09:00 entry nor swallow the 15th's.
            builder
                .push(" WHERE substr(start, 1, 10) >= ")
                .push_bind(start.to_string());
            has_where = true;
        }
        if let Some(end) = end {
            builder.push(if has_where { " AND " } else { " WHERE " });
            builder
                .push("substr(start, 1, 10) <= ")
                .push_bind(end.to_string());
        }
        builder.push(" ORDER BY start LIMIT ");
        builder.push_bind(limit as i64);
        builder
            .build_query_as::<CalendarRow>()
            .fetch_all(&self.pool)
            .await
            .map_err(Into::into)
    }
}
