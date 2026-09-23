//! 目标循环：把「没做完就接着做」交给**人**来启动。
//!
//! 一条目标（自然语言条件）挂在一个会话上。主循环停下来时，不直接收工，而是叫一个
//! **判断器**看一遍：目标达成了吗？没达成，就把理由当成下一条用户消息，**回到同一个
//! 会话**再跑一轮。这正是 Claude Code 与 deepseek-harness 各自的做法，三处约束照抄：
//!
//! * **判断器只用便宜模型、没有工具、只看对话**：让它看一眼就说结论，不许它自己动手。
//!   判断器一旦有工具，它就成了第二个执行体 —— 而它的判断本来就不该有副作用。
//! * **轮次有上限**：`JOY_GOAL_MAX_ROUNDS`（默认 5）。超了就置 `round-limit`，
//!   **如实告诉用户没完成**：不伪装完成、也不清掉目标 —— 记录还在，人可以看着办。
//! * **只有人能设/改/清**：模型没有 `goal/set` 这个工具，它甚至看不见这条路径。
//!   所以「模型自己给自己派活」在这里不是被禁止，而是不存在。
//!
//! 终止态（satisfied / impossible / round-limit / blocked）会记在目标上：循环不再
//! 自己继续跑，等下一句人类的话或一次 `goal/set`。否则每一轮都会把同一个判断再问一遍，
//! 白花钱还把「已经停下的东西」演成「还在跑」。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use joyczl_provider::{ContentBlock, CreateRequest, Message, Resolved};
use serde_json::Value;

/// 判断器最多看多少字符的对话记录（它是便宜模型，但也不是免费）。
const JUDGE_TRANSCRIPT_CHARS: usize = 8_000;
/// 判断器的输出上限：它只该回一个 JSON。
const JUDGE_MAX_TOKENS: i32 = 512;

#[derive(Debug, Clone)]
pub struct Goal {
    pub condition: String,
    /// 已经续了几轮（跨 turn 累计，人看得见）。
    pub rounds: i32,
    /// 终止态：`Some` 表示循环已经停了（等下一句人类的话）。
    pub status: Option<String>,
}

/// 会话 -> 目标。**同一会话同时只有一个活跃目标**。
pub type Goals = Arc<Mutex<HashMap<String, Goal>>>;

pub fn store() -> Goals {
    Arc::new(Mutex::new(HashMap::new()))
}

/// 设一个目标（`condition` 为空 = 清除）。返回设完之后的权威状态。
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

/// 还在跑的目标（终止态的不返回 —— 循环已经停了）。
pub fn active(goals: &Goals, session: &str) -> Option<Goal> {
    goals
        .lock()
        .expect("goal 锁不该中毒")
        .get(session)
        .filter(|goal| goal.status.is_none())
        .cloned()
}

/// 读一眼（含终止态的目标）—— 测试与诊断用。
#[allow(dead_code)]
pub fn read(goals: &Goals, session: &str) -> Option<Goal> {
    goals.lock().expect("goal 锁不该中毒").get(session).cloned()
}

/// 记一轮并返回累计轮数。
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

/// 盖一个终止态：循环接下来不再自己跑。
pub fn finish(goals: &Goals, session: &str, status: &str) {
    if let Some(goal) = goals.lock().expect("goal 锁不该中毒").get_mut(session) {
        goal.status = Some(status.to_string());
    }
}

/// 判断器的一句话结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judgement {
    pub ok: bool,
    pub impossible: bool,
    pub reason: String,
}

/// 叫判断器看一眼。失败是 `Err`（调用方据此停续轮并**保住目标**）。
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
        // 自相矛盾的答案 = 没想清楚。当作没达成，并把这件事说出来 ——
        // 按「达成」处理会让循环在一个说不清的目标上收工。
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
