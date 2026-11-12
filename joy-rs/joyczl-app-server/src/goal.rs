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
    let system = "你在判断一个长期目标有没有达成。只根据给你的对话记录判断 —— \
                  你不能调工具，也不该自己动手做事。\n\
                  只回一个 JSON 对象，不要别的话：\
                  {\"ok\": bool, \"impossible\": bool, \"reason\": \"一句话\"}\n\
                  ok 表示目标已经达成；impossible 表示按现有条件根本做不到。\
                  两者不能同时为真。reason 用一句话说清依据（关键证据），用中文。";
    let looked: String = transcript.chars().take(JUDGE_TRANSCRIPT_CHARS).collect();
    let user = format!("目标：{condition}\n\n对话记录：\n{looked}");

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
        .map_err(|e| format!("判断器调用失败：{e}"))?;

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
            "判断器没给出 JSON：{}",
            text.chars().take(200).collect::<String>()
        )
    })?;
    let value: Value =
        serde_json::from_str(&json).map_err(|e| format!("判断器的 JSON 读不了：{e}（{json}）"))?;

    let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let impossible = value
        .get("impossible")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("（判断器没给理由）")
        .to_string();

    if ok && impossible {
        // A self-contradictory answer = muddled thinking. Treat it as not met, and say so —
        // handling it as "met" would let the loop wrap up on an unspeakable goal.
        return Ok(Judgement {
            ok: false,
            impossible: false,
            reason: format!("判断器自相矛盾（ok 与 impossible 同时为真），当作没达成：{reason}"),
        });
    }
    Ok(Judgement {
        ok,
        impossible,
        reason,
    })
}
