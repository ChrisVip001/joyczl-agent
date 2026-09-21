//! token 估算：给「离上下文上限还有多远」一个数。
//!
//! 用途只有一个 —— 决定什么时候压缩工作记忆（`JOY_COMPACT_THRESHOLD`）。
//! **准确用量永远以 provider 回报的 `usage` 为准**（那才是计价与 trace 用的数）；
//! 这里只要量级对就够，因为它的唯一后果是「压缩早一点或晚一点发生」。
//!
//! 编码器取 cl100k（跨厂商的公共近似）：Claude、Gemini、DeepSeek 的分词器各不
//! 相同，为每家引一套词表不值得。中文在这个编码器下会被**高估**（约 1–2 token/字，
//! 原生中文分词器要更省），也就是压缩会早一点发生 —— 这个方向的误差可以接受，
//! 反过来的误差（低估 → 溢出 → 白跑一次请求）才是要避免的。
//!
//! 词表是编进二进制的（`cl100k_base_singleton`），运行时不联网、不读文件。

use crate::{ContentBlock, Message, ToolSchema};

/// 一条消息的固定开销（角色、分隔符等）。估个大概，不必精确。
const PER_MESSAGE_OVERHEAD: usize = 4;

/// 一个请求的外框开销（system 段头、工具段头等）。
const PER_REQUEST_OVERHEAD: usize = 8;

/// 一段文本的 token 数（近似）。
pub fn estimate_text(text: &str) -> usize {
    let text = text.trim();
    if text.is_empty() {
        return 0;
    }
    cl100k_base_singleton().encode_ordinary(text).len()
}

/// 一组消息的 token 数（含每条的固定开销）。
pub fn estimate_messages(messages: &[Message]) -> usize {
    messages.iter().map(estimate_message).sum()
}

/// 工具声明（名字 + 描述 + schema 文本）的 token 数。
///
/// 工具多起来时这块不小（MCP 接上十几个工具就是几千 token），而它**是**吃掉
/// 上下文的一部分 —— 不把它算进去，压缩就会来得太晚。
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
                ContentBlock::Text { text } => estimate_text(text),
                ContentBlock::ToolUse { name, input, .. } => {
                    estimate_text(name) + estimate_text(&input.to_string())
                }
                ContentBlock::ToolResult { content, .. } => estimate_text(content),
            })
            .sum::<usize>()
}

use tiktoken_rs::cl100k_base_singleton;
