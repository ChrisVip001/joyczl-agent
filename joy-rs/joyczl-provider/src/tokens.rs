//! Token estimation: a number for "how far from the context cap".
//!
//! One use — deciding when to compact working memory (`JOY_COMPACT_THRESHOLD`).
//! **Accurate usage always comes from the provider-reported `usage`** (the number for
//! pricing and traces); this only needs the magnitude right: its sole effect is compaction timing.
//!
//! The encoder is cl100k (a common cross-vendor approximation): Claude, Gemini, and
//! DeepSeek tokenizers all differ, and a vocabulary per vendor is not worth it. Chinese is
//! **overestimated** here (about 1–2 tokens/char; native tokenizers are leaner), so
//! compaction comes a bit earlier — acceptable; the reverse (overflow, wasted requests) is not.
//!
//! The vocabulary is compiled into the binary (`cl100k_base_singleton`); no network, no files.

use crate::{ContentBlock, Message, ToolSchema};

/// Fixed overhead per message (role, separators, etc.). A rough guess, not exact.
const PER_MESSAGE_OVERHEAD: usize = 4;

/// Envelope overhead per request (system section header, tools section header, etc.).
const PER_REQUEST_OVERHEAD: usize = 8;

/// Token count of a text (approximate).
pub fn estimate_text(text: &str) -> usize {
    let text = text.trim();
    if text.is_empty() {
        return 0;
    }
    cl100k_base_singleton().encode_ordinary(text).len()
}

/// Token count of a set of messages (including per-message overhead).
pub fn estimate_messages(messages: &[Message]) -> usize {
    messages.iter().map(estimate_message).sum()
}

/// Token count of tool declarations (name + description + schema text).
///
/// This chunk is not small once tools pile up (a dozen MCP tools is thousands of tokens),
/// and it **is** part of what eats the context — leave it out and compaction comes too late.
pub fn estimate_tools(tools: &[ToolSchema]) -> usize {
    if tools.is_empty() {
        return 0;
    }
    PER_REQUEST_OVERHEAD
        + tools
            .iter()
            .map(|tool| {
                estimate_text(&tool.name)
                    + estimate_text(&tool.description)
                    + estimate_text(&tool.input_schema.to_string())
            })
            .sum::<usize>()
}

fn estimate_message(message: &Message) -> usize {
    PER_MESSAGE_OVERHEAD
        + message
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } | ContentBlock::OpenAiReasoning { text } => {
                    estimate_text(text)
                }
                ContentBlock::ResponseItem { item } => estimate_text(&item.to_string()),
                ContentBlock::ToolUse { name, input, .. } => {
                    estimate_text(name) + estimate_text(&input.to_string())
                }
                ContentBlock::ToolResult { content, .. } => estimate_text(content),
                // A rough estimate from the raw byte count (base64, header stripped). A photo-sized image
                // lands in the 1–3k token range (Anthropic counts (w×h)/750); pixel dimensions are not
                // available here, so bytes are the best proxy — and clamping keeps a small icon from
                // rounding to zero and a huge image from skewing the compaction timing beyond recognition.
                ContentBlock::Image { data, .. } => (data.len() * 3 / 4 / 256).clamp(200, 8_000),
            })
            .sum::<usize>()
}

use tiktoken_rs::cl100k_base_singleton;
