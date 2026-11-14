//! Persistent extraction batches: retry the same source set without merging new chats.
use super::Chat;
use anyhow::Result;

impl Chat {
    /// A ready recovered batch ignores the new-work threshold. Fresh rows form one batch.
    pub async fn claim_consolidation(
        &self,
        minimum_rows: i64,
    ) -> Result<Vec<(i64, String, String)>> {
        let mut rows: Vec<(i64, String, String)> = sqlx::query_as(
            "WITH prior AS MATERIALIZED (
                SELECT consolidation_batch AS batch,1 AS existing FROM chat_log
                WHERE consolidated=0 AND consolidation_batch IS NOT NULL
                GROUP BY consolidation_batch
                HAVING MAX(COALESCE(consolidation_next_at,''))<=datetime('now')
                ORDER BY MIN(id) LIMIT 1
             ), fresh AS MATERIALIZED (
                SELECT id FROM chat_log WHERE consolidated=0 AND consolidation_batch IS NULL
                AND (consolidation_next_at IS NULL OR consolidation_next_at<=datetime('now'))
                ORDER BY id LIMIT 4096
             ), chosen AS MATERIALIZED (
                SELECT batch,existing FROM prior UNION ALL
                SELECT lower(hex(randomblob(16))),0 WHERE NOT EXISTS(SELECT 1 FROM prior)
                AND (SELECT COUNT(*) FROM fresh)>=?
             )
             UPDATE chat_log SET consolidated=1,consolidation_claimed_at=datetime('now'),
                consolidation_batch=(SELECT batch FROM chosen)
             WHERE consolidated=0 AND (
                consolidation_batch=(SELECT batch FROM chosen)
                OR (consolidation_batch IS NULL AND (SELECT existing FROM chosen)=0
                    AND id IN (SELECT id FROM fresh)))
             RETURNING id,role,content",
        )
        .bind(minimum_rows.max(1))
        .fetch_all(&self.pool)
        .await?;
        rows.sort_by_key(|row| row.0);
        Ok(rows)
    }

    /// Called only at exclusive runtime startup, before any memory worker can run.
    pub async fn recover_consolidation_claims(&self) -> Result<u64> {
        Ok(sqlx::query("UPDATE chat_log SET consolidated=0,consolidation_claimed_at=NULL WHERE consolidation_claimed_at IS NOT NULL")
            .execute(&self.pool).await?.rows_affected())
    }

    /// Clear admission markers only after every derived write has completed.
    pub async fn complete_consolidation(&self, ids: &[i64]) -> Result<()> {
        anyhow::ensure!(ids.len() <= 4096, "Too many consolidation source rows");
        if ids.is_empty() {
            return Ok(());
        }
        let sql = format!(
            "UPDATE chat_log SET consolidation_claimed_at=NULL WHERE id IN ({})",
            vec!["?"; ids.len()].join(",")
        );
        let mut query = sqlx::query(&sql);
        for id in ids {
            query = query.bind(id);
        }
        query.execute(&self.pool).await?;
        Ok(())
    }
}
