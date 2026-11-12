//! The append-only conversation ledger: what actually happened in a turn, structured.
//!
//! `chat_log` is the **view** — one line per speaker, tool activity folded into
//! `[tools used: name(前200字符)]` before it ever lands. The ledger is the **record**:
//! every user message, every assistant text block, every tool call with its arguments,
//! every tool result, every image. Rebuilding a turn from these rows is the point; the
//! view can be regenerated from the record, never the other way round.
//!
//! Three rules:
//! 1. **No UPDATE path for content.** Rows are inserted; a correction is a new row.
//!    (The `consolidated*` columns on `ledger_turns` are queue state, not content.)
//! 2. **Fidelity is declared, never guessed.** Rows backfilled from `chat_log` are
//!    `folded`: their tool outputs were truncated to 200 chars when written, so an
//!    exact rebuild of those turns is impossible — and saying so beats pretending.
//! 3. **Big payloads live outside the row** (`<home>/ledger/…`, written by the caller):
//!    the row keeps a pointer, a digest and a byte count, so verification still works.

use anyhow::Result;
use sha2::{Digest as _, Sha256};
use sqlx::sqlite::SqlitePool;

/// What one ledger row can be. Anything else is refused at the write entry — the same
/// discipline `facts::normalise_kind` uses, so readers never re-check.
pub const KINDS: [&str; 6] = [
    "user",
    "assistant",
    "tool_use",
    "tool_result",
    "image",
    "notice",
];

/// The turn was recorded from live events: every payload is the real thing.
pub const EXACT: &str = "exact";
/// The turn was backfilled from `chat_log`, whose tool outputs were already truncated.
pub const FOLDED: &str = "folded";

/// One event to append. `payload` is JSON the caller already canonicalised
/// (`serde_json::Value` sorts keys, so equal content hashes equal).
#[derive(Debug, Clone)]
pub struct NewEntry {
    pub kind: &'static str,
    pub payload: String,
    pub fidelity: &'static str,
}

impl NewEntry {
    pub fn exact(kind: &'static str, payload: impl Into<String>) -> Self {
        Self {
            kind: normalise_kind(kind),
            payload: payload.into(),
            fidelity: EXACT,
        }
    }

    /// A row whose content is known to be lossy (backfill from the folded view).
    pub fn folded(kind: &'static str, payload: impl Into<String>) -> Self {
        Self {
            kind: normalise_kind(kind),
            payload: payload.into(),
            fidelity: FOLDED,
        }
    }
}

/// Collapse anything unknown into `notice`: a row must still be storable, but the
/// readers get a closed set.
pub fn normalise_kind(kind: &str) -> &'static str {
    let trimmed = kind.trim().to_lowercase();
    KINDS
        .iter()
        .find(|known| **known == trimmed)
        .copied()
        .unwrap_or("notice")
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct EntryRow {
    pub seq: i64,
    pub kind: String,
    pub payload: String,
    pub digest: String,
    pub fidelity: String,
    pub at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct TurnRow {
    pub session_id: String,
    pub turn_id: String,
    pub turn_digest: String,
    pub events: i64,
    pub fidelity: String,
    pub preview: String,
    pub at: String,
    pub consolidated: bool,
}

/// sha256 as lowercase hex. Hex is hand-rolled: the workspace has no hex crate and this
/// is twelve characters of arithmetic versus a new dependency.
pub fn digest_of(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The per-turn digest: hashing the ordered row digests, not the payloads again — so
/// reordering, dropping or editing any row changes it, and verifying a turn costs one
/// pass over small strings.
fn turn_digest_of(entries: &[(String, String)]) -> String {
    let mut joined = String::new();
    for (kind, digest) in entries {
        joined.push_str(kind);
        joined.push('|');
        joined.push_str(digest);
        joined.push('\n');
    }
    digest_of(joined.as_bytes())
}

#[derive(Clone)]
pub struct Ledger {
    pool: SqlitePool,
}

impl Ledger {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// The pool underneath. The app itself only writes through `append`; the tamper test
    /// needs a way *around* that API to prove the digest catches an edit, and the writer
    /// needs one for its `BEGIN` boundary.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Append one turn: N event rows plus its summary row, in a single transaction.
    ///
    /// **Idempotent on `(session_id, turn_id)`**: appending the same turn twice returns
    /// the first digest and writes nothing (a crashed writer retried by the caller must
    /// not double the record). Returns the turn digest.
    pub async fn append(
        &self,
        session_id: &str,
        turn_id: &str,
        entries: &[NewEntry],
        preview: &str,
    ) -> Result<String> {
        let mut tx = self.pool.begin().await?;

        if let Some(existing) = sqlx::query_scalar::<_, String>(
            "SELECT turn_digest FROM ledger_turns WHERE session_id = ? AND turn_id = ?",
        )
        .bind(session_id)
        .bind(turn_id)
        .fetch_optional(&mut *tx)
        .await?
        {
            tx.rollback().await?;
            return Ok(existing);
        }

        let mut digests: Vec<(String, String)> = Vec::with_capacity(entries.len());
        let mut fidelity = EXACT;
        for entry in entries {
            let digest = digest_of(entry.payload.as_bytes());
            sqlx::query(
                "INSERT INTO ledger (session_id, turn_id, kind, payload, digest, fidelity)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(session_id)
            .bind(turn_id)
            .bind(entry.kind)
            .bind(&entry.payload)
            .bind(&digest)
            .bind(entry.fidelity)
            .execute(&mut *tx)
            .await?;
            if entry.fidelity == FOLDED {
                fidelity = FOLDED;
            }
            digests.push((entry.kind.to_string(), digest));
        }

        let turn_digest = turn_digest_of(&digests);
        sqlx::query(
            "INSERT INTO ledger_turns (session_id, turn_id, turn_digest, events, fidelity, preview)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(session_id)
        .bind(turn_id)
        .bind(&turn_digest)
        .bind(entries.len() as i64)
        .bind(fidelity)
        .bind(preview.chars().take(200).collect::<String>())
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(turn_digest)
    }

    /// One turn's events, in append order — the rebuild path.
    pub async fn entries(&self, session_id: &str, turn_id: &str) -> Result<Vec<EntryRow>> {
        let rows = sqlx::query_as::<_, EntryRow>(
            "SELECT seq, kind, payload, digest, fidelity, at
             FROM ledger WHERE session_id = ? AND turn_id = ? ORDER BY seq",
        )
        .bind(session_id)
        .bind(turn_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// The newest turns of one session (newest first) — what the memory center lists and
    /// what `load_history` walks when it needs structure instead of the folded view.
    pub async fn turns(&self, session_id: &str, limit: u32) -> Result<Vec<TurnRow>> {
        let rows = sqlx::query_as::<_, TurnRow>(
            "SELECT session_id, turn_id, turn_digest, events, fidelity, preview, at, consolidated
             FROM ledger_turns WHERE session_id = ?
             ORDER BY at DESC, rowid DESC LIMIT ?",
        )
        .bind(session_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Turns nobody has distilled yet, oldest first — the worker's reading order.
    pub async fn pending(&self, limit: u32) -> Result<Vec<TurnRow>> {
        let rows = sqlx::query_as::<_, TurnRow>(
            "SELECT session_id, turn_id, turn_digest, events, fidelity, preview, at, consolidated
             FROM ledger_turns
             WHERE consolidated = 0
               AND (consolidation_next_at IS NULL OR consolidation_next_at <= datetime('now'))
             ORDER BY at, rowid LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Atomically claim `limit` pending turns (mark first, process after — the same
    /// pattern `chat_log::claim_unconsolidated` uses, and for the same reason: two
    /// processes sharing state.db must not both call the model on the same rows).
    pub async fn claim_pending(&self, limit: u32) -> Result<Vec<TurnRow>> {
        let rows = sqlx::query_as::<_, TurnRow>(
            "UPDATE ledger_turns SET consolidated = 1
             WHERE (session_id, turn_id) IN (
                 SELECT session_id, turn_id FROM ledger_turns
                 WHERE consolidated = 0
                   AND (consolidation_next_at IS NULL OR consolidation_next_at <= datetime('now'))
                 ORDER BY at, rowid LIMIT ?
             )
             RETURNING session_id, turn_id, turn_digest, events, fidelity, preview, at, consolidated",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Hand a claim back after a failure, with the same 60·2ⁿ (≤1h) backoff the chat_log
    /// path uses — one backoff policy in the project, not two.
    pub async fn mark_failed(&self, session_id: &str, turn_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE ledger_turns
             SET consolidation_tries = consolidation_tries + 1,
                 consolidation_next_at = datetime('now', ?),
                 consolidated = 0
             WHERE session_id = ? AND turn_id = ?",
        )
        .bind(format!(
            "+{} seconds",
            backoff_seconds(
                sqlx::query_scalar::<_, i64>(
                    "SELECT consolidation_tries FROM ledger_turns
                     WHERE session_id = ? AND turn_id = ?",
                )
                .bind(session_id)
                .bind(turn_id)
                .fetch_optional(&self.pool)
                .await?
                .unwrap_or(0)
            )
        ))
        .bind(session_id)
        .bind(turn_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// How many turns have been recorded in total (the worker's "is there anything new?").
    pub async fn count(&self) -> Result<i64> {
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger_turns")
            .fetch_one(&self.pool)
            .await?;
        Ok(total)
    }

    /// Verify a turn against its recorded digest: `true` = the rows are exactly what was
    /// appended (same kinds, same order, same payloads).
    pub async fn verify(&self, session_id: &str, turn_id: &str) -> Result<bool> {
        let rows = self.entries(session_id, turn_id).await?;
        if rows.is_empty() {
            return Ok(false);
        }
        let recomputed = turn_digest_of(
            &rows
                .iter()
                .map(|row| (row.kind.clone(), digest_of(row.payload.as_bytes())))
                .collect::<Vec<_>>(),
        );
        let stored: Option<String> = sqlx::query_scalar(
            "SELECT turn_digest FROM ledger_turns WHERE session_id = ? AND turn_id = ?",
        )
        .bind(session_id)
        .bind(turn_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(stored.as_deref() == Some(recomputed.as_str()))
    }
}

fn backoff_seconds(tries: i64) -> i64 {
    (60i64 * 2i64.pow(tries.min(6) as u32)).min(3_600)
}

/// What a backfill did, so the caller can say it out loud.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BackfillReport {
    pub sessions: usize,
    pub turns: usize,
    pub rows: usize,
}

/// Turn the folded `chat_log` view into ledger rows, marked `folded`.
///
/// This runs **once**, on the first start after migration 0020, and again only if asked
/// (`joy memory backfill`). Two honesties are built in:
///
/// * A turn is whatever `chat_log` says a turn is — a `user` row and the `assistant` rows
///   after it. Steer messages are user rows too, so a steered turn comes back as more
///   than one turn. Saying so beats inventing a structure the data does not have.
/// * Every row gets `fidelity = folded`: those tool outputs were truncated to 200 chars
///   when they were written, so this is a *record of a record*, not a rebuild source.
///
/// Idempotent: turn ids derive from the first row id of the turn, and `append` skips a
/// turn it already has.
pub async fn backfill_from_chat(pool: &SqlitePool) -> Result<BackfillReport> {
    let rows = sqlx::query_as::<_, (i64, String, String, String)>(
        "SELECT id, COALESCE(session_id, 'default'), role, content
         FROM chat_log ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(BackfillReport::default());
    }

    let ledger = Ledger::new(pool.clone());
    let mut report = BackfillReport::default();
    let mut session_now = String::new();
    let mut turn_id = String::new();
    let mut turn_rows: Vec<NewEntry> = Vec::new();
    let mut turn_preview = String::new();

    for (id, session_id, role, content) in rows {
        let starts_turn = role == "user" || turn_rows.is_empty() || session_id != session_now;
        if starts_turn && !turn_rows.is_empty() {
            let digest = ledger
                .append(&session_now, &turn_id, &turn_rows, &turn_preview)
                .await?;
            let _ = digest;
            report.turns += 1;
            report.rows += turn_rows.len();
            turn_rows.clear();
        }
        if starts_turn {
            session_now = session_id.clone();
            turn_id = format!("foldback-{id}");
            turn_preview = content.lines().next().unwrap_or_default().to_string();
        }
        let payload = serde_json::json!({ "text": content }).to_string();
        turn_rows.push(NewEntry::folded(
            if role == "user" { "user" } else { "assistant" },
            payload,
        ));
    }

    if !turn_rows.is_empty() {
        ledger
            .append(&session_now, &turn_id, &turn_rows, &turn_preview)
            .await?;
        report.turns += 1;
        report.rows += turn_rows.len();
    }
    report.sessions = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(DISTINCT COALESCE(session_id, 'default')) FROM chat_log",
    )
    .fetch_one(pool)
    .await? as usize;
    Ok(report)
}
