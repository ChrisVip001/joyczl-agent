//! HERO MOMENT #1 — the gate deciding "does this turn consult memory at all".
//!
//! The audience's top question: "why hit the memory store every turn?" Full retrieval
//! is slow (one more search per reply) and worse: irrelevant memories drag answers.
//!
//! So before touching any store, a cheap small model answers one question:
//!   does this message need the user's memory?
//! "what's 2+2" → no. "when's my meeting with Ah Ming" → yes, and that's the query.
//!
//! Cost: one small-model call (a few hundred tokens). Gain: retrieve only when it helps.

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
    /// The query when retrieve is true; on fail-open it falls back to the message itself.
    pub query: String,
    /// Short reason. On fail-open it carries the cause, visible when debugging.
    pub reason: String,
}

impl Decision {
    /// **Fail-open**: if the gate itself breaks, assume retrieval is needed.
    /// Stale memory beats lost memory — a deliberate choice, not an oversight.
    ///
    /// Public so the caller can also fail open on a *timeout*: a gate that never answers is
    /// the same fault as one that errored, and the turn should not lose memory over it.
    pub fn fail_open(reason: impl Into<String>, message: &str) -> Self {
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
        cache_prefix: None,
        model: small_model.to_string(),
        system: None,
        messages: vec![Message::user_text(prompt)],
        tools: vec![],
        // Generous headroom: reasoning models (Kimi K3 …) emit thinking before the
        // JSON; 100 tokens would truncate the answer inside the thinking.
        max_tokens: 600,
    };

    let response = match client.create(request).await {
        Ok(response) => response,
        Err(e) => {
            return Decision::fail_open(
                format!("the gate call failed, failing open ({e})"),
                message,
            )
        }
    };

    let text = response.text();
    let Some(json_text) = extract_json(&text) else {
        // Only thinking, no JSON: not an error, the model gave no answer — fail-open too.
        return Decision::fail_open("the gate returned no JSON — failing open", message);
    };

    let Ok(value) = serde_json::from_str::<Value>(&json_text) else {
        return Decision::fail_open("the gate JSON could not be parsed — failing open", message);
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

/// Extracts the first `{...}` from a model reply.
/// Reasoning models often wrap JSON in prose, so the whole reply can't be assumed JSON.
pub fn extract_json(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end >= start).then(|| text[start..=end].to_string())
}
