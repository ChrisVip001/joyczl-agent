//! judge — the referee is not a contestant.
//!
//! Deterministic evals can only assert "were tools emitted in order"; "was the answer
//! good" has no single right answer, so follow MT-Bench / Chatbot-Arena practice: one
//! LLM scores 0-10 by rubric plus a one-line reason. **The referee must not be the
//! model under test**, or it grades itself — small model by default, `JOY_JUDGE_MODEL` overrides.
//!
//! Needs a real key. Without one the whole judge skips with an honest note — it scores, it doesn't block release.

use anyhow::Result;
use joyczl_provider::{CreateRequest, Message, Provider};

use crate::scenario::JudgeScenario;

const RUBRIC: &str = r#"You are a strict, fair judge scoring an AI assistant's reply.

The user asked:
{task}

The assistant replied:
{reply}

Score how well the reply serves the user's request on a 0-10 scale:
- 9-10: fully addresses the request, correct, concise, honest about any limits.
- 5-8: mostly addresses it, minor gaps, padding, or small errors.
- 1-4: partial, vague, or partly wrong.
- 0: ignores the request, or fabricates an action it never took.

Judge criteria for this case:
{criteria}

Reply with ONLY a JSON object, no prose:
{{"score": <int 0-10>, "reason": "<one short sentence>"}}"#;

/// Scores one real answer. Returns (score, reason).
pub async fn grade(
    client: &dyn Provider,
    model: &str,
    scenario: &JudgeScenario,
    reply: &str,
) -> Result<(i64, String)> {
    let prompt = RUBRIC
        .replace("{task}", &scenario.message)
        .replace("{reply}", reply)
        .replace("{criteria}", &scenario.judge.criteria);
    let response = client
        .create(CreateRequest {
            cache_prefix: None,
            model: model.to_string(),
            system: None,
            messages: vec![Message::user_text(prompt)],
            tools: Vec::new(),
            // Scoring is a few sentences, but reasoning models need a moment to think.
            max_tokens: 600,
        })
        .await
        .map_err(|e| anyhow::anyhow!("裁判调用失败：{e}"))?;
    parse_score(&response.text())
}

fn parse_score(text: &str) -> Result<(i64, String)> {
    // The referee may add prose around it — extract that brace pair (same leniency as the retrieval gate).
    let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) else {
        anyhow::bail!("裁判没回 JSON：{text}");
    };
    if end < start {
        anyhow::bail!("裁判没回 JSON：{text}");
    }
    let verdict: serde_json::Value = serde_json::from_str(&text[start..=end])
        .map_err(|e| anyhow::anyhow!("裁判的 JSON 坏了：{e}"))?;
    let score = verdict
        .get("score")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| anyhow::anyhow!("裁判的 JSON 里没有 score：{verdict}"))?;
    let reason = verdict
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok((score, reason))
}
