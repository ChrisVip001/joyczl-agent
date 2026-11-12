//! Whole, immutable plan versions committed together with their readable steps.
use anyhow::{ensure, Result};
use sqlx::SqlitePool;

#[derive(Clone)]
pub struct Plans {
    pool: SqlitePool,
}
impl Plans {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn latest(&self, task_id: &str) -> Result<Option<(i32, Vec<String>)>> {
        let row: Option<(i32, String)> = sqlx::query_as(
            "SELECT version, steps_json FROM task_plan_snapshots WHERE task_id=? ORDER BY version DESC LIMIT 1",
        ).bind(task_id).fetch_optional(&self.pool).await?;
        row.map(|(version, json)| Ok((version, serde_json::from_str(&json)?)))
            .transpose()
    }

    pub async fn save(&self, task_id: &str, version: i32, steps: &[String]) -> Result<()> {
        ensure!(
            version > 0 && !steps.is_empty() && steps.len() <= 12,
            "invalid plan snapshot"
        );
        ensure!(
            steps.iter().all(|s| !s.trim().is_empty()),
            "empty plan step"
        );
        let json = serde_json::to_string(steps)?;
        let mut tx = self.pool.begin().await?;
        let inserted=sqlx::query("INSERT INTO task_plan_snapshots(task_id,version,steps_json) VALUES(?,?,?) ON CONFLICT DO NOTHING")
            .bind(task_id).bind(version).bind(&json).execute(&mut *tx).await?.rows_affected();
        if inserted == 0 {
            let old: String = sqlx::query_scalar(
                "SELECT steps_json FROM task_plan_snapshots WHERE task_id=? AND version=?",
            )
            .bind(task_id)
            .bind(version)
            .fetch_one(&mut *tx)
            .await?;
            ensure!(old == json, "plan version is immutable");
        } else {
            for (index, title) in steps.iter().enumerate() {
                let input = serde_json::json!({"title":title,"version":version}).to_string();
                let key = format!("snapshot-plan:{task_id}:v{version}:{}", index + 1);
                sqlx::query("INSERT INTO task_steps(task_id,seq,kind,input_json,idempotency_key,status,note,ended_at) VALUES(?,?,'plan',?,?,'planned',?,datetime('now'))")
                    .bind(task_id).bind((index+1) as i32).bind(input).bind(key).bind(format!("v{version}")).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }
}
