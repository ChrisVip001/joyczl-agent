//! Validate durable constraints before any planning, retrieval, or execution model call.
use crate::{EventSink, Server};
use joyczl_agent::verify::Acceptance;
use joyczl_protocol::{codes, BudgetView, ErrorObject};
use joyczl_state::TaskRow;

pub(crate) fn validate(task: &TaskRow) -> Result<Vec<Acceptance>, String> {
    let acceptance = joyczl_agent::verify::parse_checked(&task.acceptance_json)?;
    let budget: BudgetView = serde_json::from_str(&task.budget_json)
        .map_err(|error| format!("Invalid task budget: {error}"))?;
    for (name, value) in [
        ("wallMs", budget.wall_ms),
        ("tokens", budget.tokens),
        ("tools", budget.tools),
    ] {
        if value.is_some_and(|value| value <= 0) {
            return Err(format!("Task budget {name} must be greater than zero"));
        }
    }
    if task.max_rounds < 1 {
        return Err("Task round limit must be greater than zero".into());
    }
    if let Some(reason) = crate::turn::budget_overrun(task) {
        return Err(reason);
    }
    Ok(acceptance)
}

pub(crate) async fn admit(
    server: &Server,
    sink: &EventSink,
    task: &TaskRow,
) -> Result<Vec<Acceptance>, ErrorObject> {
    let validation = match validate(task) {
        Ok(acceptance) => {
            match joyczl_state::external_waits::ExternalWaits::new(server.pool.clone())
                .expired(&task.id)
                .await
            {
                Ok(false) => Ok(acceptance),
                Ok(true) => Err(
                    "Expected input did not arrive before its deadline; human review is required"
                        .into(),
                ),
                Err(error) => Err(format!("Cannot verify task input deadline: {error}")),
            }
        }
        Err(error) => Err(error),
    };
    match validation {
        Ok(acceptance) => Ok(acceptance),
        Err(reason) => {
            eprintln!("(joy) task {} admission stopped: {reason}", task.id);
            if let Err(error) = server.tasks.needs_attention(&task.id, &reason).await {
                eprintln!("(joy) task admission stop could not be recorded: {error}");
            }
            if let Ok(Some(current)) = server.tasks.get(&task.id).await {
                crate::task::notify(sink, &current, Some(&reason));
            }
            Err(ErrorObject {
                code: codes::INVALID_PARAMS,
                message: format!("Task execution stopped: {reason}"),
                data: None,
            })
        }
    }
}

/// Charge known checkpoint token usage through the same cumulative accounting watermark.
pub(crate) async fn charge_checkpoint(
    server: &Server,
    row: &joyczl_state::CheckpointRow,
    usage: joyczl_provider::Usage,
) -> Result<(), String> {
    let Some(run_id) = &row.run_id else {
        return Ok(());
    };
    let run = server
        .runs
        .for_turn(&row.turn_id)
        .await
        .map_err(|error| error.to_string())?
        .filter(|run| {
            run.id == *run_id
                && run
                    .session_id
                    .as_ref()
                    .is_none_or(|session| session == &row.session_id)
        })
        .ok_or("Checkpoint usage has no matching execution identity")?;
    if row.round_id.is_none() && run.rounds > 0 {
        return Err(
            "Legacy checkpoint has no accounting identity; existing spend needs review".into(),
        );
    }
    let round_id = row
        .round_id
        .clone()
        .unwrap_or_else(|| format!("legacy:{}", row.turn_id));
    let mut account = server
        .runs
        .round_account(run_id, &round_id)
        .await
        .map_err(|error| error.to_string())?;
    if account.tokens_in >= usage.input_tokens && account.tokens_out >= usage.output_tokens {
        return Ok(());
    }
    account.tokens_in = account.tokens_in.max(usage.input_tokens);
    account.tokens_out = account.tokens_out.max(usage.output_tokens);
    server
        .runs
        .account_round(run_id, &round_id, &account)
        .await
        .map_err(|error| error.to_string())
}

/// Legacy empty/partial objects mean zero omitted counters, not malformed counter values.
pub(crate) fn checkpoint_usage(json: &str) -> Result<joyczl_provider::Usage, String> {
    let fields: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(json).map_err(|error| format!("Invalid checkpoint usage: {error}"))?;
    let count = |name: &str| -> Result<i64, String> {
        match fields.get(name) {
            None => Ok(0),
            Some(value) => value.as_i64().filter(|value| *value >= 0).ok_or_else(|| {
                format!("Invalid checkpoint {name}: expected a nonnegative integer")
            }),
        }
    };
    Ok(joyczl_provider::Usage {
        input_tokens: count("input_tokens")?,
        output_tokens: count("output_tokens")?,
        cache_read_input_tokens: count("cache_read_input_tokens")?,
        cache_creation_input_tokens: count("cache_creation_input_tokens")?,
    })
}

pub(crate) async fn stop_recovery(
    server: &Server,
    sink: &EventSink,
    row: &joyczl_state::CheckpointRow,
    reason: &str,
) {
    let charged = match checkpoint_usage(&row.usage_json) {
        Ok(usage) => charge_checkpoint(server, row, usage).await,
        Err(error) => Err(error),
    };
    let reason = match charged {
        Ok(()) => reason.to_string(),
        Err(error) => format!("{reason}; known checkpoint usage could not be charged: {error}"),
    };
    eprintln!("(joy) recovery stopped: {reason}");
    if let Err(error) = server
        .checkpoints
        .review(&row.turn_id, row.run_id.as_deref(), &reason)
        .await
    {
        eprintln!("(joy) recovery review could not be committed: {error}");
    }
    if let Some(id) = &row.run_id {
        if let Ok(Some(run)) = server.runs.get(id).await {
            if let Ok(Some(task)) = server.tasks.get(&run.task_id).await {
                crate::task::notify(sink, &task, task.last_stop_reason.as_deref());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::checkpoint_usage;

    #[test]
    fn stored_usage_accepts_legacy_counters_but_rejects_corruption() {
        assert_eq!(checkpoint_usage("{}").unwrap().input_tokens, 0);
        let usage = checkpoint_usage(r#"{"input_tokens":12,"cache_read_input_tokens":3}"#).unwrap();
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.cache_read_input_tokens, 3);
        for invalid in [
            "null",
            "[]",
            "broken",
            r#"{"input_tokens":-1}"#,
            r#"{"output_tokens":"ten"}"#,
        ] {
            assert!(checkpoint_usage(invalid).is_err(), "{invalid}");
        }
    }
}
