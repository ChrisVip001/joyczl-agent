//! Consolidation —— 把对话蒸馏成长期记忆，但只在攒够了之后。
//!
//! 白板上的那个菱形："only consolidate after N new chats"。每条消息都跑一次
//! 总结既浪费又吵；攒 N 轮才给总结器足够上下文，让它能分清「值得记一个月的」
//! 和「闲聊」。
//!
//! 产出两样：
//!   * facts   → 语义记忆（"阿明喜欢早上的会议"）
//!   * episode → 情景记忆（"2026-09-19：和阿明定了 Acme 演示"）
//!
//! **失败不丢数据**：提炼失败就让这些行保持未提炼状态，下次再试。
//! 标记 `consolidated = 1` 只在成功之后做 —— 顺序不能反。

use anyhow::Result;
use chrono::Local;
use joyczl_provider::{CreateRequest, Message, Provider};
use joyczl_state::{Chat, Episodes, Facts};
use serde_json::Value;

pub const SUMMARIZER_PROMPT: &str = "\
You distill a personal assistant's recent conversation into long-term memory.

From the exchanges below, extract:
1. durable facts about the user, their people, projects, or preferences —
   only things worth remembering in a month; skip chit-chat and one-offs.
2. one single-sentence episode summarizing what happened in this conversation.

Reply with ONLY this JSON:
{\"facts\": [{\"subject\": \"<who/what>\", \"content\": \"<one sentence>\"}], \"episode\": \"<one sentence>\"}

Exchanges:
{log}";

pub async fn consolidate_if_due(
    chat: &Chat,
    facts: &Facts,
    episodes: &Episodes,
    client: &dyn Provider,
    small_model: &str,
    every_n: i32,
) -> Result<usize> {
    let rows = chat.unconsolidated().await?;
    // 一轮对话 = 2 行（user + assistant）。
    if rows.len() < every_n.max(1) as usize * 2 {
        return Ok(0);
    }

    let log = rows
        .iter()
        .map(|(_, role, content)| format!("{role}: {content}"))
        .collect::<Vec<_>>()
        .join("\n");

    let request = CreateRequest {
        model: small_model.to_string(),
        system: None,
        messages: vec![Message::user_text(SUMMARIZER_PROMPT.replace("{log}", &log))],
        tools: vec![],
        // 预算给足：这个 prompt 装着整个未提炼的日志（不像 gate 只有一条短消息）。
        // 实测 600 会把 kimi-k2.6 截成只剩思考块（stop_reason=max_tokens、零文本）。
        max_tokens: 4096,
    };

    let text = match client.create(request).await {
        Ok(response) => response.text(),
        // 模型挂了：留着这批行，下次再试。
        Err(_) => return Ok(0),
    };

    let Some(json_text) = crate::gate::extract_json(&text) else {
        return Ok(0);
    };
    let Ok(distilled) = serde_json::from_str::<Value>(&json_text) else {
        return Ok(0);
    };

    let mut written = 0usize;
    if let Some(list) = distilled.get("facts").and_then(|f| f.as_array()) {
        for fact in list {
            let subject = fact.get("subject").and_then(|v| v.as_str()).map(str::trim);
            let content = fact.get("content").and_then(|v| v.as_str()).map(str::trim);
            if let (Some(subject), Some(content)) = (subject, content) {
                if subject.is_empty() || content.is_empty() {
                    continue;
                }
                // 临时性的陈述不进长期记忆 ——「这次会话先这样」不是关于
                // 世界的事实，记住它等于往档案里塞草稿。
                if let Some(marker) = temporary_marker(content) {
                    eprintln!("(joy) 跳过一条临时陈述（命中 '{marker}'）：{content}");
                    continue;
                }
                facts.add(subject, content, "consolidation").await?;
                written += 1;
            }
        }
    }

    if let Some(episode) = distilled
        .get("episode")
        .and_then(|v| v.as_str())
        .map(str::trim)
    {
        if !episode.is_empty() {
            let today = Local::now().format("%Y-%m-%d").to_string();
            episodes.add(&today, episode).await?;
        }
    }

    // 只在全部写成功之后才标记。
    let ids: Vec<i64> = rows.iter().map(|(id, _, _)| *id).collect();
    chat.mark_consolidated(&ids).await?;

    Ok(written)
}

/// 临时陈述的标记词。**中英并列**：模型用哪种语言提炼，就用哪种语言标记
/// 临时性，过滤得两边都认。命中就跳过 —— 「这次会话」「暂时」「for now」
/// 这类话是关于当下安排的，不是关于世界的事实。
const TEMPORARY_MARKERS: &[&str] = &[
    "本次会话",
    "这次会话",
    "本次对话",
    "这次对话",
    "当前对话",
    "暂时",
    "临时",
    "先这样",
    "就这一次",
    "仅限今天",
    "this session",
    "this conversation",
    "in this chat",
    "for now",
    "for the time being",
    "temporarily",
    "just for today",
    "only for today",
    "as a one-off",
];

pub(crate) fn temporary_marker(content: &str) -> Option<&'static str> {
    let lower = content.to_lowercase();
    TEMPORARY_MARKERS
        .iter()
        .find(|marker| lower.contains(&marker.to_lowercase()))
        .copied()
}
