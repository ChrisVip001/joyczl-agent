//! Execution plans for long tasks (P1): the steps are written down, so they can be read and
//! changed, instead of living only in the model's head.
//!
//! Three rules keep this from becoming a second execution loop:
//!
//! * **Opt-in** (`JOY_PLAN`): planning costs one model call per task and changes the prompt.
//!   A capability is switched on by hand, like every other one here.
//! * **The plan is a ledger, not an executor**: it is stored as `task_steps` rows with
//!   `kind = "plan"` and injected into the prompt as text. The loop still runs tools and the
//!   verifier still decides done — a plan step is never "executed" by this module.
//! * **Re-planning is capped** ([`MAX_VERSIONS`]): a plan that keeps being rewritten after every
//!   failed round is thrash, not progress. After the cap the loop keeps going on the last plan.
//!
//! The plan never carries acceptance conditions: those are the human's facts, and a model must
//! not be able to soften them by re-planning.

use joyczl_provider::{ContentBlock, CreateRequest, Message, Resolved};
use joyczl_state::Steps;
use serde_json::Value;

/// A plan only needs a few lines; a long one is a sign the model is padding.
pub const MAX_STEPS: usize = 12;
/// Initial plan + one re-plan. Beyond that the work is changing, not the plan.
pub const MAX_VERSIONS: i32 = 2;
const PLAN_MAX_TOKENS: i32 = 700;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub version: i32,
    pub steps: Vec<String>,
}

impl Plan {
    /// What the model sees each round. Deliberately plain text: the plan is context, not a
    /// protocol object, and a model that ignores it loses nothing but its own checklist.
    pub fn brief(&self) -> String {
        let mut text = format!(
            "Execution plan (v{}, {} steps):\n",
            self.version,
            self.steps.len()
        );
        for (index, step) in self.steps.iter().enumerate() {
            text.push_str(&format!("{}. {}\n", index + 1, step));
        }
        text.push_str(
            "Work through the plan; if a step cannot be done, say so — do not skip acceptance to reach the end.",
        );
        text
    }
}

/// Ask for a plan (or a revised one when `feedback` says what failed).
pub async fn make(
    resolved: &Resolved,
    title: &str,
    goal: Option<&str>,
    acceptance: &[String],
    feedback: Option<&str>,
    version: i32,
) -> Result<Plan, String> {
    let system = "You are planning a long-running task. Reply with one JSON object and nothing else: \
                  {\"steps\": [\"one sentence per step\", …]}\n\
                  Requirements: 3 to 6 steps; each one short, actionable, and clearly done or not done; \
                  no tool names, do not restate the acceptance criteria, and no filler like \"check whether it is done\". Write in English.";
    let mut user = format!("Task: {title}\n");
    if let Some(goal) = goal {
        user.push_str(&format!("Goal: {goal}\n"));
    }
    if !acceptance.is_empty() {
        user.push_str("Acceptance criteria (set by the user, not negotiable):\n");
        for line in acceptance {
            user.push_str(&format!("- {line}\n"));
        }
    }
    if let Some(feedback) = feedback {
        user.push_str(&format!(
            "\nThe previous plan did not get past this: {feedback}\nGive a revised plan (v{version})."
        ));
    }

    let response = resolved
        .client
        .create(CreateRequest {
            cache_prefix: None,
            model: resolved.model.clone(),
            system: Some(system.to_string()),
            messages: vec![Message::user_text(user)],
            tools: Vec::new(),
            max_tokens: PLAN_MAX_TOKENS,
        })
        .await
        .map_err(|e| format!("the planner call failed: {e}"))?;

    let text = response
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let json = joyczl_memory::gate::extract_json(&text).ok_or_else(|| {
        format!(
            "the planner returned no JSON: {}",
            text.chars().take(200).collect::<String>()
        )
    })?;
    let value: Value = serde_json::from_str(&json)
        .map_err(|e| format!("the plan JSON could not be read: {e} ({json})"))?;

    let steps: Vec<String> = value
        .get("steps")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|step| !step.is_empty())
                .take(MAX_STEPS)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if steps.is_empty() {
        return Err("the planner returned an empty plan (steps is empty)".to_string());
    }
    Ok(Plan { version, steps })
}

/// Commit the complete recovery snapshot and readable step rows together.
pub async fn record(steps: &Steps, task_id: &str, plan: &Plan) -> Result<(), String> {
    steps
        .plans()
        .save(task_id, plan.version, &plan.steps)
        .await
        .map_err(|e| e.to_string())
}

/// Legacy partial step rows are deliberately not treated as complete snapshots.
pub async fn restore(steps: &Steps, task_id: &str) -> Result<Option<Plan>, String> {
    steps
        .plans()
        .latest(task_id)
        .await
        .map(|row| row.map(|(version, steps)| Plan { version, steps }))
        .map_err(|e| e.to_string())
}
