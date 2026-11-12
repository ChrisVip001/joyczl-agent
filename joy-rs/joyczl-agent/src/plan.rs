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
use joyczl_state::{NewStep, Steps};
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
            "执行计划（第 {} 版，共 {} 步）：\n",
            self.version,
            self.steps.len()
        );
        for (index, step) in self.steps.iter().enumerate() {
            text.push_str(&format!("{}. {}\n", index + 1, step));
        }
        text.push_str("按计划推进；哪一步做不到就直说，不要为了走完计划而跳过验收条件。");
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
    let system = "你在给一个长期任务排执行计划。只回一个 JSON 对象，不要别的话：\
                  {\"steps\": [\"一步一句话\", …]}\n\
                  要求：3 到 6 步；每一步都短、可执行、能看出做没做完；\
                  不要写工具名，不要复述验收条件，不要写「检查是否完成」这种空话。用中文。";
    let mut user = format!("任务：{title}\n");
    if let Some(goal) = goal {
        user.push_str(&format!("目标：{goal}\n"));
    }
    if !acceptance.is_empty() {
        user.push_str("完成标准（由人定，不能改）：\n");
        for line in acceptance {
            user.push_str(&format!("- {line}\n"));
        }
    }
    if let Some(feedback) = feedback {
        user.push_str(&format!(
            "\n上一版计划跑到这里没过：{feedback}\n请给出调整后的计划（第 {version} 版）。"
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
        .map_err(|e| format!("计划器调用失败：{e}"))?;

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
            "计划器没给出 JSON：{}",
            text.chars().take(200).collect::<String>()
        )
    })?;
    let value: Value =
        serde_json::from_str(&json).map_err(|e| format!("计划的 JSON 读不了：{e}（{json}）"))?;

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
        return Err("计划器给了空计划（steps 为空）".to_string());
    }
    Ok(Plan { version, steps })
}

/// Write the plan into the step ledger. `run_id` stays `None`: a plan outlives the run that
/// produced it, and `ToolsOk` counts by tool name, so plan rows can never inflate a tool count.
pub async fn record(steps: &Steps, task_id: &str, plan: &Plan) {
    for (index, step) in plan.steps.iter().enumerate() {
        let input = serde_json::json!({ "title": step, "version": plan.version }).to_string();
        let key = format!("plan:{task_id}:v{}:{}", plan.version, index + 1);
        match steps
            .begin(NewStep {
                task_id,
                run_id: None,
                seq: (index + 1) as i32,
                kind: "plan",
                tool: None,
                input_json: Some(&input),
                idempotency_key: Some(&key),
            })
            .await
        {
            Ok(id) => {
                if let Err(e) = steps
                    .close(
                        id,
                        "planned",
                        None,
                        Some(&format!("第 {} 版", plan.version)),
                    )
                    .await
                {
                    eprintln!("(joy) 计划步骤收尾没记上（不影响回合）：{e}");
                }
            }
            Err(e) => eprintln!("(joy) 计划步骤没记进台账：{e}"),
        }
    }
}
