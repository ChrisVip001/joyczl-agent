//! Human memory edits and suppression of automatically re-extracted old claims.
use super::{normalise_kind, FactRow, Facts};
use anyhow::Result;
use sqlx::{Sqlite, Transaction};

fn key(subject: &str, content: Option<&str>) -> String {
    let values = (
        subject.trim().to_ascii_lowercase(),
        content.map(|text| text.trim().to_ascii_lowercase()),
    );
    crate::ledger::digest_of(
        serde_json::to_string(&values)
            .expect("strings serialize")
            .as_bytes(),
    )
}

async fn suppress(
    tx: &mut Transaction<'_, Sqlite>,
    subject: &str,
    content: Option<&str>,
    reason: &str,
    replacement: Option<i64>,
) -> Result<()> {
    sqlx::query("INSERT INTO memory_suppressions(key,scope,reason,replacement_id) VALUES(?,?,?,?) ON CONFLICT(key) DO UPDATE SET reason=excluded.reason,replacement_id=excluded.replacement_id,created_at=datetime('now')")
        .bind(key(subject, content)).bind(if content.is_some() { "fact" } else { "subject" }).bind(reason).bind(replacement).execute(&mut **tx).await?;
    Ok(())
}

pub(super) async fn add(
    facts: &Facts,
    subject: &str,
    content: &str,
    source: &str,
    kind: &str,
) -> Result<Option<(FactRow, bool)>> {
    add_from(facts, subject, content, source, kind, &[]).await
}

pub(super) async fn add_from(
    facts: &Facts,
    subject: &str,
    content: &str,
    source: &str,
    kind: &str,
    source_refs: &[String],
) -> Result<Option<(FactRow, bool)>> {
    add_observed(
        facts,
        subject,
        content,
        source,
        kind,
        Observation {
            refs: source_refs,
            transient: None,
        },
    )
    .await
}

pub(super) struct Observation<'a> {
    pub refs: &'a [String],
    pub transient: Option<&'a super::TransientObservation>,
}

pub(super) async fn add_observed(
    facts: &Facts,
    subject: &str,
    content: &str,
    source: &str,
    kind: &str,
    observation: Observation<'_>,
) -> Result<Option<(FactRow, bool)>> {
    let source_refs = observation.refs;
    anyhow::ensure!(
        source_refs.len() <= 4096,
        "Too many memory source references"
    );
    let mut refs = source_refs.to_vec();
    refs.sort();
    refs.dedup();
    anyhow::ensure!(
        refs.iter()
            .all(|value| !value.is_empty() && value.len() <= 256),
        "Invalid memory source reference"
    );
    let refs_json = serde_json::to_string(&refs)?;
    let context = crate::ledger::digest_of(refs_json.as_bytes());
    let subject_key = key(subject, None);
    let fact_key = key(subject, Some(content));
    let mut tx = facts.pool.begin().await?;
    if source == "user" {
        sqlx::query("DELETE FROM memory_suppressions WHERE key=?")
            .bind(&fact_key)
            .execute(&mut *tx)
            .await?;
    }
    let inserted = sqlx::query_as::<_, FactRow>(
        "INSERT INTO facts(subject,content,source,kind,last_confirmed_at,confirm_count)
         SELECT ?,?,?,?,datetime('now'),1 WHERE ?='user' OR NOT EXISTS(SELECT 1 FROM memory_suppressions WHERE key IN (?,?))
         ON CONFLICT DO NOTHING RETURNING id,subject,content,source,kind,created_at")
        .bind(subject).bind(content).bind(source).bind(normalise_kind(kind)).bind(source).bind(&subject_key).bind(&fact_key).fetch_optional(&mut *tx).await?;
    // The INSERT acquired the writer lock: source validation and admission cannot race edits.
    let prepared = match observation.transient {
        Some(observation) => match observation.prepare(&mut tx, &refs).await? {
            Some(prepared) => Some(prepared),
            None => return Ok(None),
        },
        None => None,
    };
    if let Some(row) = inserted {
        if !refs.is_empty() {
            record_observation(&mut tx, row.id, &context, &refs_json).await?;
        }
        if let Some(prepared) = &prepared {
            prepared.record(&mut tx, row.id, &context).await?;
        }
        tx.commit().await?;
        return Ok(Some((row, true)));
    }
    let blocked: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM memory_suppressions WHERE key IN (?,?))")
            .bind(&subject_key)
            .bind(&fact_key)
            .fetch_one(&mut *tx)
            .await?;
    if blocked && source != "user" {
        tx.commit().await?;
        return Ok(None);
    }
    let existing = sqlx::query_as::<_, FactRow>("SELECT id,subject,content,source,kind,created_at FROM facts WHERE subject=? AND lower(trim(content))=lower(trim(?)) ORDER BY id LIMIT 1")
        .bind(subject).bind(content).fetch_one(&mut *tx).await?;
    if prepared.is_some() {
        let origin: Option<String> =
            sqlx::query_scalar("SELECT origin FROM memory_validity WHERE fact_id=?")
                .bind(existing.id)
                .fetch_optional(&mut *tx)
                .await?;
        if existing.source != "consolidation" || origin.as_deref() != Some("consolidation") {
            eprintln!(
                "(joy) transient extraction cannot override an unbounded or human-edited memory"
            );
            return Ok(None);
        }
    }
    let new_context =
        refs.is_empty() || record_observation(&mut tx, existing.id, &context, &refs_json).await?;
    if new_context {
        if let Some(prepared) = &prepared {
            prepared.record(&mut tx, existing.id, &context).await?;
        }
        sqlx::query("UPDATE facts SET confirm_count=confirm_count+1,last_confirmed_at=datetime('now') WHERE id=?")
            .bind(existing.id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(Some((existing, false)))
}

async fn record_observation(
    tx: &mut Transaction<'_, Sqlite>,
    id: i64,
    context: &str,
    refs_json: &str,
) -> Result<bool> {
    Ok(sqlx::query("INSERT INTO memory_observations(fact_id,context_digest,source_refs_json) VALUES(?,?,?) ON CONFLICT DO NOTHING")
        .bind(id).bind(context).bind(refs_json).execute(&mut **tx).await?.rows_affected() == 1)
}

pub(super) async fn delete(facts: &Facts, id: i64) -> Result<bool> {
    let mut tx = facts.pool.begin().await?;
    let row: Option<(String, String)> =
        sqlx::query_as("DELETE FROM facts WHERE id=? RETURNING subject,content")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some((subject, content)) = &row {
        suppress(&mut tx, subject, Some(content), "deleted", None).await?;
    }
    tx.commit().await?;
    Ok(row.is_some())
}

pub(super) async fn forget_subject(facts: &Facts, subject: &str) -> Result<u64> {
    let mut tx = facts.pool.begin().await?;
    suppress(&mut tx, subject, None, "forgotten_subject", None).await?;
    let count = sqlx::query("DELETE FROM facts WHERE subject=?")
        .bind(subject)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(count)
}

pub(super) async fn update(facts: &Facts, id: i64, content: &str) -> Result<bool> {
    let mut tx = facts.pool.begin().await?;
    // Acquire the writer lock before reading the old claim.
    let row: Option<(String, String)> =
        sqlx::query_as("UPDATE facts SET id=id WHERE id=? RETURNING subject,content")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((subject, previous)) = row else {
        return Ok(false);
    };
    if key(&subject, Some(&previous)) != key(&subject, Some(content)) {
        suppress(&mut tx, &subject, Some(&previous), "corrected", Some(id)).await?;
    }
    sqlx::query("DELETE FROM memory_suppressions WHERE key=?")
        .bind(key(&subject, Some(content)))
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM memory_observations WHERE fact_id=?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE memory_validity SET origin='human' WHERE fact_id=?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE facts SET content=?,source='user',embedding=NULL,last_confirmed_at=datetime('now'),confirm_count=1 WHERE id=?")
        .bind(content).bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}
