//! Human-controlled time bounds. Expiration filters recall without deleting history.
use super::Facts;
use anyhow::{ensure, Result};

impl Facts {
    /// Replace the whole validity window; null bounds restore an unbounded claim.
    /// Timestamps must be RFC3339 and are normalized before entering SQLite.
    pub async fn set_validity(
        &self,
        id: i64,
        from: Option<&str>,
        until: Option<&str>,
    ) -> Result<bool> {
        let parse = |value: Option<&str>| -> Result<Option<String>> {
            value
                .map(|text| {
                    chrono::DateTime::parse_from_rfc3339(text)
                        .map(|time| time.to_utc().to_rfc3339())
                        .map_err(Into::into)
                })
                .transpose()
        };
        let from = parse(from)?;
        let until = parse(until)?;
        let mut tx = self.pool.begin().await?;
        let exists = sqlx::query("UPDATE facts SET id=id WHERE id=?")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            == 1;
        if !exists {
            return Ok(false);
        }
        if let (Some(start), Some(end)) = (&from, &until) {
            let valid: bool = sqlx::query_scalar("SELECT julianday(?)<julianday(?)")
                .bind(start)
                .bind(end)
                .fetch_one(&mut *tx)
                .await?;
            ensure!(valid, "Memory validity end must be after its start");
        }
        sqlx::query("INSERT INTO memory_validity(fact_id,valid_from,valid_until) VALUES(?,?,?) ON CONFLICT(fact_id) DO UPDATE SET valid_from=excluded.valid_from,valid_until=excluded.valid_until,origin='human',updated_at=datetime('now')")
            .bind(id).bind(from).bind(until).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn validity(&self, id: i64) -> Result<(Option<String>, Option<String>)> {
        Ok(
            sqlx::query_as("SELECT valid_from,valid_until FROM memory_validity WHERE fact_id=?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?
                .unwrap_or_default(),
        )
    }

    /// Static pages may only expose still-present, unbounded facts.
    pub async fn is_durable(&self, id: i64) -> Result<bool> {
        Ok(
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM durable_facts WHERE id=?)")
                .bind(id)
                .fetch_one(&self.pool)
                .await?,
        )
    }
}
