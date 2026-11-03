//! The goal loop: handing "keep going until it is done" to **a human** to start.
//!
//! One goal (a natural-language condition) hangs on a session. When the main loop stops, instead of
//! wrapping up it calls a **judge** to look once: is the goal met? If not, the reason becomes the next
//! user message and another round runs **in the same session**. Three constraints:
//!
//! * **The judge uses only the cheap model, has no tools, and only reads the conversation**: it looks
//!   once and states a conclusion; it is not allowed to act. With tools it becomes a second executor — and its judgement should never have side effects.
//! * **Rounds are capped**: `JOY_GOAL_MAX_ROUNDS` (default 5). Beyond that the status becomes `round-limit`,
//!   **telling the user honestly it is not done**: no faking completion, no clearing the goal — the record stays, the human decides.
//! * **Only a human can set/change/clear**: the model has no `goal/set` tool; it cannot even see this
//!   path. So "the model assigning itself work" is not forbidden here — it does not exist.
//!
//! Terminal states (satisfied / impossible / round-limit / blocked) are recorded on the goal: the loop
//! no longer continues on its own, waiting for the next human message or a `goal/set`. Otherwise every turn
//! would re-ask the same judgement, wasting money while staging "already stopped" as "still running".

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use joyczl_provider::{ContentBlock, CreateRequest, Message, Resolved};
use serde_json::Value;

/// How many characters of transcript the judge may read at most (a cheap model, but not free).
const JUDGE_TRANSCRIPT_CHARS: usize = 8_000;
/// The judge's output cap: it should only return one JSON.
const JUDGE_MAX_TOKENS: i32 = 512;

#[derive(Debug, Clone)]
pub struct Goal {
    pub condition: String,
    /// How many rounds have been continued (accumulated across turns, visible to the human).
    pub rounds: i32,
    /// Terminal state: `Some` means the loop has stopped (waiting for the next human message).
    pub status: Option<String>,
}

/// Session -> goal. **At most one active goal per session at a time**.
pub type Goals = Arc<Mutex<HashMap<String, Goal>>>;

pub fn store() -> Goals {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Set a goal (empty `condition` = clear). Returns the authoritative state after setting.
pub fn set(goals: &Goals, session: &str, condition: Option<&str>) -> Option<Goal> {
    let mut table = goals.lock().expect("goal 锁不该中毒");
    match condition.map(str::trim).filter(|text| !text.is_empty()) {
        Some(condition) => {
            let goal = Goal {
                condition: condition.to_string(),
                rounds: 0,
                status: None,
            };
            table.insert(session.to_string(), goal.clone());
            Some(goal)
        }
        None => {
            table.remove(session);
            None
        }
    }
}

/// The still-running goal (terminal-state goals are not returned — the loop has stopped).
pub fn active(goals: &Goals, session: &str) -> Option<Goal> {
    goals
        .lock()
        .expect("goal 锁不该中毒")
        .get(session)
        .filter(|goal| goal.status.is_none())
        .cloned()
}

/// Peek (including terminal-state goals) — for tests and diagnostics.
#[allow(dead_code)]
pub fn read(goals: &Goals, session: &str) -> Option<Goal> {
    goals.lock().expect("goal 锁不该中毒").get(session).cloned()
}

/// Record one round and return the accumulated count.
pub fn bump(goals: &Goals, session: &str) -> i32 {
    let mut table = goals.lock().expect("goal 锁不该中毒");
    match table.get_mut(session) {
        Some(goal) => {
            goal.rounds += 1;
            goal.rounds
        }
        None => 0,
    }
}

/// Stamp a terminal state: the loop no longer runs on its own from here.
pub fn finish(goals: &Goals, session: &str, status: &str) {
    if let Some(goal) = goals.lock().expect("goal 锁不该中毒").get_mut(session) {
        goal.status = Some(status.to_string());
    }
}

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
