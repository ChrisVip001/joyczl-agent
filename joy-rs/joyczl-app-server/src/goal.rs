//! The judge: the one call that decides whether "keep going until X" has arrived.
//!
//! This module owns the **question**, not the state. The loop lives in `turn.rs`; the goal lives
//! on the task row (`agent_tasks.goal_condition` / `rounds_used` / `max_rounds`). There used to be
//! an in-process table here mirroring the row — a second copy of the truth, restored at boot and
//! kept in step by hand. It is gone (0.17): "set a goal" and "create a task" are the same act, so
//! there is one place to read and nothing to reconcile.
//!
//! Three constraints survive from the original design:
//!
//! * **The judge uses only the cheap model, has no tools, and only reads the conversation**: it looks
//!   once and states a conclusion; it is not allowed to act. With tools it becomes a second executor — and its judgement should never have side effects.
//! * **Rounds are capped**: `JOY_GOAL_MAX_ROUNDS` (default 5). Beyond that the status becomes `round-limit`,
//!   **telling the user honestly it is not done**: no faking completion, no clearing the goal — the record stays, the human decides.
//! * **Only a human can set/change/clear**: the model has no `/goal` tool; it cannot even see this
//!   path. So "the model assigning itself work" is not forbidden here — it does not exist.
//!
//! A terminal verdict (satisfied / impossible / round-limit / blocked) is recorded on the task row,
//! not on a goal object: the loop no longer continues on its own, it waits for the next human
//! message or a `/goal`. Otherwise every turn would re-ask the same judgement, wasting money while
//! staging "already stopped" as "still running".

use joyczl_provider::{ContentBlock, CreateRequest, Message, Resolved};
use serde_json::Value;

/// How many characters of transcript the judge may read at most (a cheap model, but not free).
const JUDGE_TRANSCRIPT_CHARS: usize = 8_000;
/// The judge's output cap: it should only return one JSON.
const JUDGE_MAX_TOKENS: i32 = 512;

/// The judge's one-sentence conclusion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judgement {
    pub ok: bool,
    pub impossible: bool,
    pub reason: String,
}

/// Ask the judge to look once. Failure is an `Err` (the caller stops continuing and **keeps the goal**).
pub async fn judge(
    resolved: &Resolved,
    condition: &str,
    transcript: &str,
) -> Result<Judgement, String> {
    let system = "You are deciding whether a long-running goal has been met. Judge only from the conversation record you were given — you cannot call tools, and you should not do the work yourself.\nReply with one JSON object and nothing else:\n{\"ok\": bool, \"impossible\": bool, \"reason\": \"one sentence\"}\nok means the goal has been met; impossible means it cannot be done under the current conditions. They cannot both be true. reason states the evidence in one sentence, in English.";
    let looked: String = transcript.chars().take(JUDGE_TRANSCRIPT_CHARS).collect();
    let user = format!("Goal: {condition}\n\nConversation record:\n{looked}");

    let response = resolved
        .client
        .create(CreateRequest {
            cache_prefix: None,
            model: resolved.small_model.clone(),
            system: Some(system.to_string()),
            messages: vec![Message::user_text(user)],
            tools: Vec::new(),
            max_tokens: JUDGE_MAX_TOKENS,
        })
        .await
        .map_err(|e| format!("the judge call failed: {e}"))?;

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
            "the judge returned no JSON: {}",
            text.chars().take(200).collect::<String>()
        )
    })?;
    let value: Value = serde_json::from_str(&json)
        .map_err(|e| format!("the judge's JSON could not be read: {e} ({json})"))?;

    let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let impossible = value
        .get("impossible")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("(the judge gave no reason)")
        .to_string();

    if ok && impossible {
        // A self-contradictory answer = muddled thinking. Treat it as not met, and say so —
        // handling it as "met" would let the loop wrap up on an unspeakable goal.
        return Ok(Judgement {
            ok: false,
            impossible: false,
            reason: format!("the judge contradicted itself (ok and impossible both true); counting it as not met: {reason}"),
        });
    }
    Ok(Judgement {
        ok,
        impossible,
        reason,
    })
}
