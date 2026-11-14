//! Rebuild durable task context from normalized records, independently of chat summaries.
use crate::Server;
use joyczl_protocol::{codes, ErrorObject};
use serde_json::{json, Value};

fn failed(error: impl std::fmt::Display) -> ErrorObject {
    ErrorObject {
        code: codes::INTERNAL_ERROR,
        message: format!("Cannot assemble durable task handoff; execution stopped: {error}"),
        data: None,
    }
}

pub(crate) async fn load(
    server: &Server,
    turn_id: &str,
    session_id: &str,
) -> Result<String, ErrorObject> {
    let Some(run) = server.runs.for_turn(turn_id).await.map_err(failed)? else {
        if server
            .checkpoints
            .get(turn_id)
            .await
            .map_err(failed)?
            .is_some_and(|checkpoint| checkpoint.run_id.is_some())
        {
            return Err(failed("checkpoint run has no matching execution identity"));
        }
        return Ok(String::new());
    };
    if run
        .session_id
        .as_deref()
        .is_some_and(|owner| owner != session_id)
    {
        return Err(failed("execution session does not match the run"));
    }
    let task = server
        .tasks
        .get(&run.task_id)
        .await
        .map_err(failed)?
        .ok_or_else(|| failed("task is missing"))?;
    crate::task::contract::validate(&task).map_err(failed)?;
    let acceptance: Value = serde_json::from_str(&task.acceptance_json).map_err(failed)?;
    let budget: Value = serde_json::from_str(&task.budget_json).map_err(failed)?;
    let previous_checks: Value = serde_json::from_str(&task.checks_json).map_err(failed)?;
    let plan = joyczl_agent::plan::restore(&server.steps, &task.id)
        .await
        .map_err(failed)?
        .map(|plan| json!({"version":plan.version,"steps":plan.steps}));
    let steps = server.steps.list_by_run(&run.id).await.map_err(failed)?;
    let recent_steps:Vec<Value>=steps.into_iter().rev().take(16).map(|step|json!({
        "sequence":step.seq,"tool":step.tool,"status":step.status,"operationKey":step.idempotency_key
    })).collect();
    let artifacts = server.artifacts.list(&task.id, 16).await.map_err(failed)?;
    let artifacts:Vec<Value>=artifacts.into_iter().map(|artifact|json!({
        "path":artifact.path,"recordedBytes":artifact.bytes,"recordedAt":artifact.at,"runId":artifact.run_id
    })).collect();
    let operations = joyczl_state::operations::ToolOperations::new(server.pool.clone())
        .list(&task.id)
        .await
        .map_err(failed)?;
    let operations: Vec<Value> = operations.into_iter().filter(|operation| operation.run_id.as_deref()==Some(run.id.as_str()))
        .rev().take(16).map(|operation| json!({
            "operationId":operation.id,"tool":operation.tool,"status":operation.status,
            "inputDigest":operation.input_digest,"automaticReplay":"never",
            "argumentsPreview":operation.input_json.chars().take(2048).collect::<String>(),
            "humanEvidencePreview":operation.evidence.map(|text|text.chars().take(512).collect::<String>())
        })).collect();
    let verification = joyczl_state::verification::Verifications::new(server.pool.clone())
        .latest(&task.id)
        .await
        .map_err(failed)?;
    let verification=verification.map(|receipt| -> Result<Value,ErrorObject> {
        let files:Value=serde_json::from_str(&receipt.files_json).map_err(failed)?;
        Ok(json!({"receiptId":receipt.id,"runId":receipt.run_id,"roundId":receipt.round_id,
            "measuredAt":receipt.measured_at,"previousReceiptId":receipt.previous_id,"files":files}))
    }).transpose()?;
    let input = crate::task::inputs::prompt(server, &task.id, &run.id)
        .await
        .map_err(failed)?;
    let handoff = json!({
        "schemaVersion":1,
        "externalInput":input,
        "recentOperationEvidenceNewestFirst":operations,
        "taskId":task.id,"runId":run.id,
        "contract":{"title":task.title,"goal":task.goal_condition,"acceptance":acceptance,"budget":budget,"cwd":task.cwd},
        "spent":{"tokens":task.spent_tokens,"tools":task.spent_tools,"wallMs":task.spent_wall_ms},
        "rounds":{"used":task.rounds_used,"limit":task.max_rounds},
        "plan":plan,
        "previousMeasurements":previous_checks,
        "historicalVerificationReceipt":verification,
        "recentStepsNewestFirst":recent_steps,
        "recordedArtifactsNotRevalidated":artifacts,
        "previousStopReason":task.last_stop_reason,
        "nextAction":"Continue the authorized contract from checkpointed progress. Recheck current artifacts and acceptance. Unknown effects require reconciliation, never an automatic repeat."
    });
    let text = serde_json::to_string(&handoff).map_err(failed)?;
    if text.len() > 64_000 {
        return Err(failed(
            "handoff exceeds 64000 bytes; shorten the contract or review its evidence",
        ));
    }
    Ok(format!("\nDurable task handoff (schema 1, reconstructed from persistent records):\n{text}\nThe contract is independent of rolling summaries. Prior measurements and recorded artifacts are historical evidence, not current verification. Ledger operation keys do not prove external idempotency. This context never grants additional tool permissions.\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_constraints_or_foreign_execution_context_are_rejected() {
        let home = tempfile::tempdir().unwrap();
        let pool = joyczl_state::open(&home.path().join("state.db"))
            .await
            .unwrap();
        let server = Server::boot(
            pool.clone(),
            joyczl_config::Settings {
                home: home.path().into(),
                ..Default::default()
            },
        )
        .await;
        server
            .tasks
            .create(joyczl_state::NewTask {
                id: "task",
                session_id: Some("owner"),
                title: "contract",
                goal_condition: None,
                acceptance_json: "[]",
                budget_json: "{}",
                cwd: None,
                max_rounds: 1,
                tier: None,
            })
            .await
            .unwrap();
        server
            .runs
            .start("run", "task", Some("work"), Some("execution"))
            .await
            .unwrap();
        assert!(load(&server, "execution", "another-work").await.is_err());
        assert!(load(&server, "ordinary-chat", "owner")
            .await
            .unwrap()
            .is_empty());
        let before = load(&server, "execution", "work").await.unwrap();
        assert!(before.contains("Durable task handoff"));
        server
            .runs
            .start("ambiguous", "task", Some("work"), Some("execution"))
            .await
            .unwrap();
        assert!(load(&server, "execution", "work").await.is_err());
        sqlx::query("DELETE FROM task_runs WHERE id='ambiguous'")
            .execute(&pool)
            .await
            .unwrap();
        for (acceptance, budget) in [
            ("null", "{}"),
            (r#"[{"kind":"fileExist","path":"report.md"}]"#, "{}"),
            ("[]", r#"{"tokens":-1}"#),
            ("[]", r#"{"tokens":"ten"}"#),
        ] {
            sqlx::query("UPDATE agent_tasks SET acceptance_json=?,budget_json=? WHERE id='task'")
                .bind(acceptance)
                .bind(budget)
                .execute(&pool)
                .await
                .unwrap();
            assert!(
                load(&server, "execution", "work").await.is_err(),
                "invalid contract: {acceptance} {budget}"
            );
        }
        sqlx::query("UPDATE agent_tasks SET acceptance_json='invalid JSON' WHERE id='task'")
            .execute(&pool)
            .await
            .unwrap();
        let error = load(&server, "execution", "work").await.unwrap_err();
        assert!(error.message.contains("execution stopped"));
    }
}
