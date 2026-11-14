//! Immutable verification receipts bind acceptance to observed file content and an exact run.
use anyhow::{ensure, Result};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, SqlitePool};
use std::{io::Read, path::Path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeasurement {
    pub path: String,
    pub bytes: i64,
    pub sha256: String,
}
impl FileMeasurement {
    pub fn from_bytes(path: &Path, bytes: &[u8]) -> Self {
        Self {
            path: path.display().to_string(),
            bytes: bytes.len() as i64,
            sha256: crate::ledger::digest_of(bytes),
        }
    }
    pub fn read_text(path: &Path) -> Result<(String, Self)> {
        Self::read_text_limited(path, u64::MAX)
    }
    pub fn read_text_limited(path: &Path, max_bytes: u64) -> Result<(String, Self)> {
        ensure!(
            std::fs::metadata(path)?.is_file(),
            "Verification target is not a regular file"
        );
        let file = std::fs::File::open(path)?;
        let before = file.metadata()?;
        ensure!(
            before.is_file(),
            "Verification target is not a regular file"
        );
        ensure!(
            before.len() <= max_bytes,
            "Verification target exceeds the byte limit"
        );
        let mut text = String::new();
        (&file)
            .take(before.len().saturating_add(1))
            .read_to_string(&mut text)?;
        let after = file.metadata()?;
        ensure!(
            text.len() as u64 == before.len()
                && before.len() == after.len()
                && before.modified()? == after.modified()?,
            "File changed while verification was reading it"
        );
        let measurement = Self::from_bytes(path, text.as_bytes());
        Ok((text, measurement))
    }
    /// Stream regular files, bounded by their original length; concurrent edits fail closed.
    pub fn read(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        let before = file.metadata()?;
        ensure!(
            before.is_file(),
            "Verification target is not a regular file"
        );
        let mut reader = (&file).take(before.len().saturating_add(1));
        let mut hash = Sha256::new();
        let mut buffer = [0_u8; 65_536];
        let mut bytes = 0_u64;
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            bytes = bytes.saturating_add(count as u64);
            hash.update(&buffer[..count]);
        }
        let after = file.metadata()?;
        ensure!(
            bytes == before.len()
                && before.len() == after.len()
                && before.modified()? == after.modified()?,
            "File changed while verification was reading it"
        );
        ensure!(bytes <= i64::MAX as u64, "File is too large to record");
        Ok(Self {
            path: path.display().to_string(),
            bytes: bytes as i64,
            sha256: format!("{:x}", hash.finalize()),
        })
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct VerificationRow {
    pub id: String,
    pub task_id: String,
    pub run_id: String,
    pub round_id: String,
    pub acceptance_json: String,
    pub checks_json: String,
    pub files_json: String,
    pub measured_at: String,
    pub previous_id: Option<String>,
}
impl VerificationRow {
    pub fn verify(&self) -> Result<()> {
        let acceptance: serde_json::Value = serde_json::from_str(&self.acceptance_json)?;
        let checks: serde_json::Value = serde_json::from_str(&self.checks_json)?;
        let files: Vec<serde_json::Value> = serde_json::from_str(&self.files_json)?;
        let payload = serde_json::to_vec(&(
            &self.task_id,
            &self.run_id,
            &self.round_id,
            &acceptance,
            &checks,
            &files,
        ))?;
        let digest = crate::ledger::digest_of(&serde_json::to_vec(&(
            self.previous_id.as_deref(),
            &self.measured_at,
            &payload,
        ))?);
        ensure!(
            digest == self.id,
            "Verification receipt digest does not match its evidence"
        );
        Ok(())
    }
}
#[derive(Clone)]
pub struct Verifications {
    pool: SqlitePool,
}
impl Verifications {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
    pub async fn latest(&self, task_id: &str) -> Result<Option<VerificationRow>> {
        let row: Option<VerificationRow> = sqlx::query_as(
            "SELECT * FROM task_verifications WHERE task_id=? ORDER BY rowid DESC LIMIT 1",
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?;
        if let Some(row) = &row {
            row.verify()?;
        }
        Ok(row)
    }
    pub async fn get(&self, id: &str) -> Result<Option<VerificationRow>> {
        let row: Option<VerificationRow> =
            sqlx::query_as("SELECT * FROM task_verifications WHERE id=?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        if let Some(row) = &row {
            row.verify()?;
        }
        Ok(row)
    }
    pub(crate) async fn require_completion(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        task_id: &str,
        run_id: Option<&str>,
    ) -> Result<()> {
        let changed = sqlx::query("UPDATE agent_tasks SET id=id WHERE id=?")
            .bind(task_id)
            .execute(&mut **tx)
            .await?
            .rows_affected();
        ensure!(changed == 1, "Completion task is unavailable");
        let (status, acceptance, checks): (String, String, String) =
            sqlx::query_as("SELECT status,acceptance_json,checks_json FROM agent_tasks WHERE id=?")
                .bind(task_id)
                .fetch_one(&mut **tx)
                .await?;
        if status == "done" || acceptance == "[]" {
            return Ok(());
        }
        let run = run_id.ok_or_else(|| {
            anyhow::anyhow!("Verified completion requires an exact execution identity")
        })?;
        let latest: Option<String> = sqlx::query_scalar(
            "SELECT id FROM task_runs WHERE task_id=? ORDER BY started_at DESC,rowid DESC LIMIT 1",
        )
        .bind(task_id)
        .fetch_optional(&mut **tx)
        .await?;
        ensure!(
            latest.as_deref() == Some(run),
            "Completion belongs to a superseded execution"
        );
        let receipt:Option<VerificationRow>=sqlx::query_as("SELECT * FROM task_verifications WHERE task_id=? AND run_id=? ORDER BY rowid DESC LIMIT 1")
            .bind(task_id).bind(run).fetch_optional(&mut **tx).await?;
        let receipt = receipt
            .ok_or_else(|| anyhow::anyhow!("Completion has no committed verification evidence"))?;
        receipt.verify()?;
        ensure!(
            receipt.acceptance_json == acceptance && receipt.checks_json == checks,
            "Completion evidence no longer matches the task contract or checks"
        );
        let checks: Vec<serde_json::Value> = serde_json::from_str(&checks)?;
        ensure!(
            !checks.is_empty()
                && checks.iter().all(|check| check
                    .get("passed")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)),
            "Completion evidence contains failed or invalid acceptance checks"
        );
        Ok(())
    }

    /// Receipt and current checks are one commit; unchanged observations reuse their receipt.
    pub async fn record(
        &self,
        task_id: &str,
        run_id: &str,
        round_id: &str,
        acceptance_json: &str,
        checks_json: &str,
        files: &[FileMeasurement],
    ) -> Result<String> {
        let acceptance: serde_json::Value = serde_json::from_str(acceptance_json)?;
        let checks: serde_json::Value = serde_json::from_str(checks_json)?;
        ensure!(
            acceptance.as_array().is_some_and(|a| !a.is_empty())
                && acceptance.as_array().map(Vec::len) == checks.as_array().map(Vec::len),
            "Verification checks do not match their acceptance contract"
        );
        let mut versions = std::collections::BTreeMap::new();
        for file in files {
            ensure!(
                file.bytes >= 0
                    && file.sha256.len() == 64
                    && file.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "Invalid artifact fingerprint"
            );
            if let Some(previous) = versions.insert(file.path.clone(), file) {
                ensure!(
                    previous == file,
                    "Acceptance checks observed different versions of the same file"
                );
            }
        }
        let files:Vec<_>=versions.values().map(|file|serde_json::json!({"path":file.path,"bytes":file.bytes,"sha256":file.sha256})).collect();
        let files_json = serde_json::to_string(&files)?;
        let payload =
            serde_json::to_vec(&(task_id, run_id, round_id, &acceptance, &checks, &files))?;
        let mut tx = self.pool.begin().await?;
        let changed=sqlx::query("UPDATE agent_tasks SET checks_json=?,updated_at=datetime('now') WHERE id=? AND acceptance_json=? AND status IN ('running','suspended','needs_attention') AND EXISTS(SELECT 1 FROM task_runs r WHERE r.id=? AND r.task_id=agent_tasks.id AND r.id=(SELECT latest.id FROM task_runs latest WHERE latest.task_id=r.task_id ORDER BY latest.started_at DESC,latest.rowid DESC LIMIT 1))")
            .bind(checks_json).bind(task_id).bind(acceptance_json).bind(run_id).execute(&mut *tx).await?.rows_affected();
        ensure!(
            changed == 1,
            "Task contract or execution changed before verification commit"
        );
        let previous:Option<VerificationRow>=sqlx::query_as("SELECT * FROM task_verifications WHERE task_id=? AND run_id=? ORDER BY rowid DESC LIMIT 1")
            .bind(task_id).bind(run_id).fetch_optional(&mut *tx).await?;
        if let Some(previous) = &previous {
            previous.verify()?;
            if previous.round_id == round_id
                && previous.acceptance_json == acceptance_json
                && previous.checks_json == checks_json
                && previous.files_json == files_json
            {
                tx.commit().await?;
                return Ok(previous.id.clone());
            }
        }
        let previous_id = previous.map(|row| row.id);
        let measured_at: String = sqlx::query_scalar("SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now')")
            .fetch_one(&mut *tx)
            .await?;
        let id = crate::ledger::digest_of(&serde_json::to_vec(&(
            previous_id.as_deref(),
            &measured_at,
            &payload,
        ))?);
        sqlx::query("INSERT INTO task_verifications(id,task_id,run_id,round_id,acceptance_json,checks_json,files_json,previous_id,measured_at) VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO NOTHING")
            .bind(&id).bind(task_id).bind(run_id).bind(round_id).bind(acceptance_json)
            .bind(checks_json).bind(&files_json).bind(&previous_id).bind(&measured_at).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(id)
    }
}
