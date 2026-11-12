//! Turning a finished turn into ledger rows — and back.
//!
//! The loop hands over its full working memory (`TurnRunOutcome::messages`: the user
//! message, every assistant block, every tool call and tool result). `chat_log` keeps
//! the folded one-line-per-speaker view; this module keeps the record, so the turn can
//! be rebuilt exactly — for the memory worker, for a stronger model re-reading history,
//! and for the memory center's "what actually happened" view.
//!
//! **Big payloads do not go into the row.** A 200 KB tool result would bloat the
//! database and every backup; instead it lands in `<home>/ledger/<session>/…` and the
//! row keeps a pointer plus the content digest, so verification still works and the
//! text is one file read away.
//!
//! Failures here never fail the turn: the reply is already persisted by the time this
//! runs, and losing the ledger row is a reporting loss, not a conversational one —
//! exactly the discipline tool errors follow.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use joyczl_provider::{ContentBlock, Message, Role};
use joyczl_state::NewEntry;
use serde_json::json;

/// Where spilled payloads live and how large a payload may be before it is spilled.
#[derive(Debug, Clone, Copy)]
pub struct LedgerSink<'a> {
    pub home: &'a Path,
    pub inline_max: usize,
}

impl<'a> LedgerSink<'a> {
    pub fn new(home: &'a Path, inline_max: usize) -> Self {
        Self { home, inline_max }
    }

    fn dir(&self) -> PathBuf {
        self.home.join("ledger")
    }
}

/// One content block's payload: either the text itself or a pointer to the spilled file.
fn payload_of(
    sink: &LedgerSink<'_>,
    session_id: &str,
    turn_id: &str,
    ordinal: usize,
    kind: &str,
    body: &str,
    always_spill: bool,
) -> String {
    if !always_spill && body.len() <= sink.inline_max {
        return body.to_string();
    }
    let ext = if kind == "tool_use" { "json" } else { "txt" };
    let name = format!("{turn_id}-{ordinal:02}-{kind}.{ext}");
    let dir = sink.dir().join(safe_component(session_id));
    let path = dir.join(&name);
    let digest = joyczl_state::ledger::digest_of(body.as_bytes());
    let bytes = body.len();
    match std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, body.as_bytes())) {
        Ok(()) => json!({
            "ptr": format!("ledger/{}/{}", safe_component(session_id), name),
            "digest": digest,
            "bytes": bytes,
            "preview": body.chars().take(200).collect::<String>(),
        })
        .to_string(),
        // Spilling failed: keep the content inline rather than lose it. A big row beats
        // a missing one — the whole point of the record is that nothing disappears.
        Err(error) => {
            eprintln!(
                "(joy) 账本转存失败，改为整段落库（{}）：{error}",
                path.display()
            );
            body.to_string()
        }
    }
}

/// Session ids are labels; keep them usable as directory names without letting `..` out.
fn safe_component(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    if cleaned.is_empty() {
        "session".to_string()
    } else {
        cleaned
    }
}

/// Build the ledger rows for one finished turn.
///
/// `user_message` is recorded once from the caller (it is also inside `messages`, so the
/// copy that matches is skipped — recording it twice would make every rebuild show the
/// request twice).
pub fn entries_for_turn(
    sink: &LedgerSink<'_>,
    session_id: &str,
    turn_id: &str,
    user_message: &str,
    messages: &[Message],
    reply: &str,
) -> Vec<NewEntry> {
    let mut entries: Vec<NewEntry> = Vec::new();
    let mut ordinal = 0usize;

    // The request itself. Every payload is JSON — one shape for readers, and the digest
    // covers canonical bytes rather than "some string that happens to be text".
    entries.push(NewEntry::exact(
        "user",
        payload_of(
            sink,
            session_id,
            turn_id,
            0,
            "user",
            &json!({ "text": user_message }).to_string(),
            false,
        ),
    ));

    let mut saw_assistant = false;
    let mut user_recorded = false;
    for message in messages {
        match message.role {
            Role::Assistant => {
                saw_assistant = true;
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } if !text.trim().is_empty() => {
                            let body = json!({ "text": text }).to_string();
                            entries.push(NewEntry::exact(
                                "assistant",
                                payload_of(
                                    sink,
                                    session_id,
                                    turn_id,
                                    ordinal,
                                    "assistant",
                                    &body,
                                    false,
                                ),
                            ));
                            ordinal += 1;
                        }
                        ContentBlock::ToolUse {
                            id, name, input, ..
                        } => {
                            let body =
                                json!({ "id": id, "name": name, "input": input }).to_string();
                            entries.push(NewEntry::exact(
                                "tool_use",
                                payload_of(
                                    sink, session_id, turn_id, ordinal, "tool_use", &body, false,
                                ),
                            ));
                            ordinal += 1;
                        }
                        // Images never appear on assistant messages today; if a provider
                        // ever sends one, recording it beats dropping it.
                        ContentBlock::Image { media_type, data } => {
                            let body = json!({ "mediaType": media_type, "data": data }).to_string();
                            entries.push(NewEntry::exact(
                                "image",
                                payload_of(
                                    sink, session_id, turn_id, ordinal, "image", &body, true,
                                ),
                            ));
                            ordinal += 1;
                        }
                        _ => {}
                    }
                }
            }
            Role::User => {
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } => {
                            // The first user text is this turn's request: already recorded.
                            if !user_recorded && text == user_message {
                                user_recorded = true;
                                continue;
                            }
                            if text.trim().is_empty() {
                                continue;
                            }
                            let body = json!({ "text": text }).to_string();
                            entries.push(NewEntry::exact(
                                "user",
                                payload_of(
                                    sink, session_id, turn_id, ordinal, "user", &body, false,
                                ),
                            ));
                            ordinal += 1;
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                        } => {
                            let body =
                                json!({ "toolUseId": tool_use_id, "content": content }).to_string();
                            entries.push(NewEntry::exact(
                                "tool_result",
                                payload_of(
                                    sink,
                                    session_id,
                                    turn_id,
                                    ordinal,
                                    "tool_result",
                                    &body,
                                    false,
                                ),
                            ));
                            ordinal += 1;
                        }
                        ContentBlock::Image { media_type, data } => {
                            let body = json!({ "mediaType": media_type, "data": data }).to_string();
                            entries.push(NewEntry::exact(
                                "image",
                                payload_of(
                                    sink, session_id, turn_id, ordinal, "image", &body, true,
                                ),
                            ));
                            ordinal += 1;
                        }
                        ContentBlock::ToolUse { .. } => {}
                    }
                }
            }
        }
    }

    // A provider that ends without a structured assistant text (interrupted turn, error
    // path) still owes the record the sentence the user saw.
    if !saw_assistant && !reply.trim().is_empty() {
        let body = json!({ "text": reply }).to_string();
        entries.push(NewEntry::exact(
            "assistant",
            payload_of(
                sink,
                session_id,
                turn_id,
                ordinal,
                "assistant",
                &body,
                false,
            ),
        ));
    }
    entries
}

/// Resolve a row's payload into text, following the pointer when it was spilled.
///
/// Wired in when the reader lands (memory worker / `JOY_HISTORY_SOURCE=ledger`); kept
/// here beside the writer so the two cannot drift while the read path is still to come.
///
/// A missing spill file is reported, never silently rendered as the pointer JSON — the
/// caller (rebuild or the memory center) needs to know the record is incomplete.
#[allow(dead_code)] // 读路径（worker / 结构化历史）落地时去掉
pub fn payload_text(home: &Path, payload: &str) -> Result<String> {
    let parsed: serde_json::Value = match serde_json::from_str(payload) {
        Ok(value) => value,
        Err(_) => return Ok(payload.to_string()),
    };
    let Some(ptr) = parsed.get("ptr").and_then(|value| value.as_str()) else {
        // Inline: the text is in the payload's `text` field when it has one.
        return Ok(parsed
            .get("text")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| payload.to_string()));
    };
    let path = home.join(ptr);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("账本内容丢了：{}", path.display()))?;
    if let Some(expected) = parsed.get("digest").and_then(|value| value.as_str()) {
        let actual = joyczl_state::ledger::digest_of(text.as_bytes());
        if actual != expected {
            anyhow::bail!("账本内容被改过：{}（摘要对不上）", path.display());
        }
    }
    Ok(text)
}

/// Rebuild one turn as provider messages — the exact path `load_history` takes in stage 3.
///
/// Text and tool rows are restored as they were; a spilled payload is read back (and
/// verified) on demand. `notice` rows and unknown kinds are skipped: they are commentary
/// about the turn, not part of the conversation.
#[allow(dead_code)] // 同上：阶段三把 load_history 切到账本时去掉
pub fn rebuild_turn(home: &Path, rows: &[joyczl_state::EntryRow]) -> Result<Vec<Message>> {
    let mut messages: Vec<Message> = Vec::new();
    for row in rows {
        let text = payload_text(home, &row.payload)?;
        match row.kind.as_str() {
            "assistant" => push_block(&mut messages, Role::Assistant, ContentBlock::Text { text }),
            "user" => push_block(&mut messages, Role::User, ContentBlock::Text { text }),
            "tool_use" => {
                let parsed: serde_json::Value = serde_json::from_str(&text)?;
                push_block(
                    &mut messages,
                    Role::Assistant,
                    ContentBlock::ToolUse {
                        id: parsed
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        name: parsed
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        input: parsed
                            .get("input")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                        extra: None,
                    },
                );
            }
            "tool_result" => {
                let parsed: serde_json::Value = serde_json::from_str(&text)?;
                push_block(
                    &mut messages,
                    Role::User,
                    ContentBlock::ToolResult {
                        tool_use_id: parsed
                            .get("toolUseId")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        content: parsed
                            .get("content")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    },
                );
            }
            _ => {}
        }
    }
    Ok(messages)
}

/// Append a block, merging into the previous message when the role matches — the wires
/// expect `assistant[tool_use…]` in one message, not one message per block.
#[allow(dead_code)] // rebuild_turn 的帮手
fn push_block(messages: &mut Vec<Message>, role: Role, block: ContentBlock) {
    match messages.last_mut() {
        Some(last) if last.role == role => last.content.push(block),
        _ => messages.push(Message {
            role,
            content: vec![block],
        }),
    }
}

#[cfg(test)]
#[path = "ledger_writer_tests.rs"]
mod ledger_writer_tests;
