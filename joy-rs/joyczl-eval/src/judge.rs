//! judge —— 裁判不是选手。
//!
//! 确定性 eval 只能断言「工具有没有按序发出」；「答得好不好」没有唯一
//! 正确答案，那就按 MT-Bench / Chatbot-Arena 的市场做法：一个 LLM 按
//! rubric 给 0-10 分加一句话理由。**裁判必须不是被考的那个模型**，
//! 否则是自己给自己打分 —— 默认用 small model，`JOY_JUDGE_MODEL` 可换。
//!
//! 需要真 key。没 key 时整个 judge 跳过并如实说明 —— 它出分，不拦发版。

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

/// 给一轮真实回答打分。返回 (score, reason)。
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
            model: model.to_string(),
            system: None,
            messages: vec![Message::user_text(prompt)],
            tools: Vec::new(),
            // 打分是几句话的事，但推理模型得先想一会儿。
            max_tokens: 600,
        })
        .await
        .map_err(|e| anyhow::anyhow!("裁判调用失败：{e}"))?;
    parse_score(&response.text())
}

fn parse_score(text: &str) -> Result<(i64, String)> {
    // 裁判可能前后带说明 —— 抠出那对大括号（跟检索门同一套宽容）。
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
