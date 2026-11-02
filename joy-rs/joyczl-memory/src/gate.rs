//! HERO MOMENT #1 —— 决定「这轮到底要不要查记忆」的门。
//!
//! 受众问得最多的问题："为什么每轮都要打一遍记忆库？" 默认全量检索
//! （a）慢 —— 每次回复前多一次搜索；（b）更糟：无关的记忆会带偏答案。
//!
//! 所以在碰任何存储之前，先让一个便宜的小模型回答一个问题：
//!   这条消息需要用户的记忆吗？
//! "2+2 等于几" → 不用。"我什么时候和阿明开会" → 用，这是检索词。
//!
//! 成本：一次小模型调用（几百 token）。收益：只在有帮助时才检索。

use joyczl_provider::{CreateRequest, Message, Provider};
use serde_json::Value;

pub const GATE_PROMPT: &str = "\
You are a retrieval gate for a personal assistant's long-term memory.
Given the user's message, decide if answering well requires the user's stored
memories (facts about people, projects, preferences, or past events).

Reply with ONLY this JSON, nothing else:
{\"retrieve\": true/false, \"query\": \"<search keywords if true, else empty>\", \"reason\": \"<5 words>\"}

General knowledge, math, small talk, or self-contained requests → false.
Anything referencing the user's life, people, plans, or history → true.

User message: {message}";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub retrieve: bool,
    /// retrieve 为真时的检索词；失败开放时退回原消息本身。
    pub query: String,
    /// 简短理由。失败开放时带上原因，排查时能看到。
    pub reason: String,
}

impl Decision {
    /// **失败开放**：门自己坏了，就当作需要检索。
    /// 过时的记忆好过丢失的记忆 —— 这是有意的选择，不是疏忽。
    fn fail_open(reason: impl Into<String>, message: &str) -> Self {
        Self {
            retrieve: true,
            query: message.to_string(),
            reason: reason.into(),
        }
    }
}

pub async fn should_retrieve(client: &dyn Provider, small_model: &str, message: &str) -> Decision {
    let prompt = GATE_PROMPT.replace("{message}", message);
    let request = CreateRequest {
        model: small_model.to_string(),
        system: None,
        messages: vec![Message::user_text(prompt)],
        tools: vec![],
        // 余量给足：推理模型（Kimi K3 …）会先吐一段思考再给 JSON，
        // 100 token 会把答案截在思考里。
        max_tokens: 600,
    };

    let response = match client.create(request).await {
        Ok(response) => response,
        Err(e) => return Decision::fail_open(format!("gate 调用失败，失败开放（{e}）"), message),
    };

    let text = response.text();
    let Some(json_text) = extract_json(&text) else {
        // 只有一段思考、没有 JSON：不是出错，是模型没给答案 —— 同样失败开放。
        return Decision::fail_open("gate 没返回 JSON — 失败开放", message);
    };

    let Ok(value) = serde_json::from_str::<Value>(&json_text) else {
        return Decision::fail_open("gate 的 JSON 解析失败 — 失败开放", message);
    };

    let retrieve = value
        .get("retrieve")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let query = value
        .get("query")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(message)
        .to_string();
    let reason = value
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    Decision {
        retrieve,
        query,
        reason,
    }
}

/// 从模型回复里抠出第一个 `{...}`。
/// 推理模型常在 JSON 前后带说明文字，所以不能假定整段就是 JSON。
pub fn extract_json(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end >= start).then(|| text[start..=end].to_string())
}
