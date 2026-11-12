//! Consolidation — distills conversations into long-term memory, but only once enough accrue.
//!
//! The diamond on the whiteboard: "only consolidate after N new chats". Summarizing
//! every message is wasteful and noisy; N accumulated turns give the summarizer
//! enough context to tell "worth keeping a month" from "chit-chat".
//!
//! Two outputs:
//!   * facts   → semantic memory ("Ah Ming likes morning meetings")
//!   * episode → episodic memory ("2026-09-19: set the Acme demo with Ah Ming")
//!
//! **Failure loses no data**: on failure the rows stay unconsolidated and are retried.
//! Marking `consolidated = 1` happens only after success — never the reverse.

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

For each fact also pick a kind:
  - \"user\"      who the user is (role, people, preferences, habits)
  - \"project\"   what is being worked on (state, decisions, deadlines)
  - \"feedback\"  how they want you to work (corrections, style, rules)
  - \"reference\"  pointers worth keeping (links, ids, commands, file paths)
  - \"fact\"      anything else

Reply with ONLY this JSON:
{\"facts\": [{\"subject\": \"<who/what>\", \"content\": \"<one sentence>\", \"kind\": \"user|project|feedback|reference|fact\"}], \"episode\": \"<one sentence>\"}

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
    let pending = chat.unconsolidated().await?;
    // One turn = 2 rows (user + assistant).
    if pending.len() < every_n.max(1) as usize * 2 {
        return Ok(0);
    }

    // **Claim**: one SQL does "select + mark", so concurrent calls have a single
    // winner. The loser gets an empty list and doesn't even call the model (see chat.rs).
    let claimed = chat.claim_unconsolidated().await?;
    let ids: Vec<i64> = claimed.iter().map(|(id, _, _)| *id).collect();
    if ids.is_empty() {
        // Someone else claimed it: a concurrent turn, or schedule and REPL firing together.
        return Ok(0);
    }
    let log = claimed
        .iter()
        .map(|(_, role, content)| format!("{role}: {content}"))
        .collect::<Vec<_>>()
        .join("\n");

    let request = CreateRequest {
        cache_prefix: None,
        model: small_model.to_string(),
        system: None,
        messages: vec![Message::user_text(SUMMARIZER_PROMPT.replace("{log}", &log))],
        tools: vec![],
        // Generous budget: this prompt carries the whole unconsolidated log (unlike
        // gate's short message); 600 in practice truncates kimi-k2.6 to thinking only.
        max_tokens: 4096,
    };

    let ids: Vec<i64> = claimed.iter().map(|(id, _, _)| *id).collect();
    // All three failure paths (model down, JSON unreadable, JSON invalid) take
    // **backoff**: reschedule the rows to a future moment. The old "next time" made
    // the same bad rows retry every turn — burning calls, and it looked stuck.
    let text = match client.create(request).await {
        Ok(response) => response.text(),
        Err(_) => {
            chat.mark_consolidation_failed(&ids).await?;
            return Ok(0);
        }
    };

    let Some(json_text) = crate::gate::extract_json(&text) else {
        chat.mark_consolidation_failed(&ids).await?;
        return Ok(0);
    };
    let Ok(distilled) = serde_json::from_str::<Value>(&json_text) else {
        chat.mark_consolidation_failed(&ids).await?;
        return Ok(0);
    };

    // Write facts and episodes. **Any failure must return the claim** — claiming marks
    // first; an error without unmarking leaves the log "consolidated" with nothing written.
    let written = match persist(&distilled, facts, episodes).await {
        Ok(written) => written,
        Err(error) => {
            chat.mark_consolidation_failed(&ids).await?;
            return Err(error);
        }
    };

    Ok(written)
}

/// Writes distilled results into facts and episodes. Returns the count of **new** facts.
async fn persist(distilled: &Value, facts: &Facts, episodes: &Episodes) -> Result<usize> {
    let mut written = 0usize;
    if let Some(list) = distilled.get("facts").and_then(|f| f.as_array()) {
        for fact in list {
            let subject = fact.get("subject").and_then(|v| v.as_str()).map(str::trim);
            let content = fact.get("content").and_then(|v| v.as_str()).map(str::trim);
            // A missing or malformed kind falls back to `fact` (funneled at the write entry, see facts.rs).
            let kind = fact.get("kind").and_then(|v| v.as_str()).unwrap_or("fact");
            if let (Some(subject), Some(content)) = (subject, content) {
                if subject.is_empty() || content.is_empty() {
                    continue;
                }
                // Temporary statements don't enter long-term memory — "leave it here for
                // this session" isn't a fact about the world; keeping it files a draft.
                if let Some(marker) = temporary_marker(content) {
                    eprintln!("(joy) 跳过一条临时陈述（命中 '{marker}'）：{content}");
                    continue;
                }
                // Duplicates don't count as written: `written` means "how many were new
                // this round"; counting them inflates the ledger and implies constant output.
                let (_, is_new) = facts.add(subject, content, "consolidation", kind).await?;
                if is_new {
                    written += 1;
                } else {
                    eprintln!("(joy) 跳过一条已经存在的事实：{subject} —— {content}");
                }
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

    Ok(written)
}

/// Markers for temporary statements. **Chinese and English side by side**: whichever
/// language the model distills in marks temporariness, so the filter accepts both. A
/// hit skips — "this session", "for now" talk is present arrangements, not world facts.
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
