//! Source-bound, short-lived observations; confidence is model-reported, not calibrated.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use sqlx::{Sqlite, Transaction};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TransientObservation {
    pub source_ref: String,
    pub quote: String,
    pub confidence: f64,
    pub ttl_hours: i32,
}

pub(super) struct Prepared {
    assessment: String,
    until: String,
}

impl TransientObservation {
    pub(super) async fn prepare(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        refs: &[String],
    ) -> Result<Option<Prepared>> {
        ensure!(
            self.confidence.is_finite() && (0.0..=1.0).contains(&self.confidence),
            "Transient confidence must be between zero and one"
        );
        ensure!(
            (1..=24).contains(&self.ttl_hours),
            "Transient lifetime must be between one and 24 hours"
        );
        ensure!(
            (4..=400).contains(&self.quote.trim().chars().count()),
            "Transient quote must contain four to 400 characters"
        );
        ensure!(
            refs.contains(&self.source_ref),
            "Transient source is outside the extraction context"
        );
        let (id, expected) = self
            .source_ref
            .strip_prefix("chat:")
            .and_then(|value| value.split_once('@'))
            .ok_or_else(|| anyhow::anyhow!("Invalid transient chat reference"))?;
        let id: i64 = id.parse()?;
        let row: Option<(String, String, String)> = sqlx::query_as("SELECT role,content,strftime('%Y-%m-%dT%H:%M:%fZ',created_at) FROM chat_log WHERE id=?")
            .bind(id).fetch_optional(&mut **tx).await?;
        let (role, content, at) =
            row.ok_or_else(|| anyhow::anyhow!("Transient source no longer exists"))?;
        let digest =
            crate::ledger::digest_of(serde_json::to_string(&(&role, &content))?.as_bytes());
        ensure!(
            role == "user" && digest == expected && content.contains(&self.quote),
            "Transient source must match the quoted user message"
        );
        let observed = chrono::DateTime::parse_from_rfc3339(&at)?.to_utc();
        ensure!(
            observed <= chrono::Utc::now(),
            "Transient source timestamp is in the future"
        );
        let until = observed + chrono::Duration::hours(i64::from(self.ttl_hours));
        if until <= chrono::Utc::now() {
            eprintln!("(joy) skipped an already expired transient observation");
            return Ok(None);
        }
        let until = until.to_rfc3339();
        let mut assessment = serde_json::to_value(self)?;
        assessment["validUntil"] = serde_json::Value::String(until.clone());
        assessment["basis"] = serde_json::Value::String(
            "model-classified user quotation; not independently verified".into(),
        );
        Ok(Some(Prepared {
            assessment: serde_json::to_string(&assessment)?,
            until,
        }))
    }
}

impl Prepared {
    pub(super) async fn record(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        id: i64,
        context: &str,
    ) -> Result<()> {
        sqlx::query("INSERT INTO memory_validity(fact_id,valid_until,origin) VALUES(?,?,'consolidation') ON CONFLICT(fact_id) DO UPDATE SET valid_until=CASE WHEN julianday(excluded.valid_until)>julianday(memory_validity.valid_until) THEN excluded.valid_until ELSE memory_validity.valid_until END,updated_at=datetime('now') WHERE memory_validity.origin='consolidation'")
            .bind(id).bind(&self.until).execute(&mut **tx).await?;
        sqlx::query("INSERT INTO memory_transient_assessments(fact_id,context_digest,assessment_json) VALUES(?,?,?) ON CONFLICT DO NOTHING")
            .bind(id).bind(context).bind(&self.assessment).execute(&mut **tx).await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "transient_memory_tests.rs"]
mod tests;
