//! 上下文压缩：滑窗之外的对话，压成一段话带回 prompt。
//!
//! 滑窗（`JOY_HISTORY_TURNS`）是硬边界：更老的轮次不再进 prompt。对短会话
//! 无所谓，对长会话就是失忆 —— 「我们上周聊过什么」从上下文里彻底消失。
//! 压缩把被挤出去的那部分**折进一段摘要**，摘要随会话存进 state.db，
//! 每轮拼在 system prompt 里。摘要是往前滚的：每次只处理新挤出去的几轮，
//! 不重算全史。
//!
//! 三条规矩，跟这个 crate 的其它成员一致：
//!
//! * **失败开放**：摘要调用失败就退回**确定性兜底**（把被挤出的轮次截断
//!   拼接），绝不因为模型不配合而丢内容 —— 摘要再糙，也好过整段对话凭空消失。
//! * **只往前滚**：已覆盖的轮次不重算。摘要是省 token 的手段，不是第二个
//!   记忆源；事实来源永远是 state.db 里的 chat_log。
//! * **可观测**：调用方拿到的是 `(覆盖轮数, 摘要)`，谁都能看出这段摘要
//!   管到哪儿为止。
//!
//! 工具输出不需要在这里修剪：写入时就经过 `fold_tool_activity` 折成了
//! 一行带截断的文本（见 app-server 的 turn.rs），历史里本来就没有巨块。

use anyhow::Result;
use joyczl_provider::{CreateRequest, Message, Provider};

/// 兜底摘要的字符上限。够放下几轮对话的骨架，又不至于自己变成上下文炸弹。
const FALLBACK_CHARS: usize = 600;

/// 摘要的提示词。要的是**留住事实**，不是文学加工。
const SUMMARIZER_PROMPT: &str = "\
You are compressing the older part of a conversation between a user and their \
assistant, so it can stay in the context window as a short paragraph. Keep \
every fact, name, decision, number and open thread. Drop pleasantries and \
repetition. Write plain prose in the language of the conversation. No \
headings, no lists, no commentary about the task itself.";

/// 把新挤出去的轮次折进已有的摘要。**永不返回 Err** —— 模型失败时
/// 用确定性兜底顶上。
///
/// 返回 `(新的覆盖轮数, 新摘要)`。`previous` 是上一版摘要（覆盖了多少轮 +
/// 正文），`newly_evicted` 是这次新挤出去的老轮次。
pub async fn roll_forward(
    client: &dyn Provider,
    model: &str,
    previous: Option<(i32, String)>,
    newly_evicted: &[(String, String)],
) -> (i32, String) {
    let (covered_before, previous_summary) = match previous {
        Some((covered, summary)) => (covered, summary),
        None => (0, String::new()),
    };
    let covered_now = covered_before + newly_evicted.len() as i32;

    // 拼给模型看的原文：上一版摘要 + 这次新挤出去的轮次。
    let mut material = String::new();
    if !previous_summary.trim().is_empty() {
        material.push_str("Earlier summary:\n");
        material.push_str(&previous_summary);
        material.push_str("\n\nNew turns to fold in:\n");
    }
    for (user, assistant) in newly_evicted {
        material.push_str(&format!("User: {user}\nAssistant: {assistant}\n"));
    }

    let request = CreateRequest {
        model: model.to_string(),
        system: Some(SUMMARIZER_PROMPT.to_string()),
        messages: vec![Message::user_text(material.clone())],
        tools: Vec::new(),
        // 摘要要短：给多了它会顺手把对话复述一遍。
        max_tokens: 700,
    };

    match client.create(request).await {
        Ok(response) => {
            let summary = response.text().trim().to_string();
            if summary.is_empty() {
                // 回了个空 —— 跟失败同等对待：兜底至少不会丢内容。
                (
                    covered_now,
                    fallback_summary(&previous_summary, newly_evicted),
                )
            } else {
                (covered_now, summary)
            }
        }
        Err(_) => (
            covered_now,
            fallback_summary(&previous_summary, newly_evicted),
        ),
    }
}

/// 确定性兜底：截断拼接。**不加工**，只保证「东西还在」。
fn fallback_summary(previous: &str, newly_evicted: &[(String, String)]) -> String {
    let mut text = String::new();
    if !previous.trim().is_empty() {
        text.push_str(previous.trim());
        text.push_str(" … ");
    }
    for (user, assistant) in newly_evicted {
        text.push_str(&format!(
            "用户：{} ｜ 助手：{} ",
            clip(user, 120),
            clip(assistant, 160)
        ));
    }
    let clipped = clip(text.trim(), FALLBACK_CHARS);
    format!("（模型摘要不可用，以下是原对话的截断摘录：{clipped}）")
}

fn clip(text: &str, max: usize) -> String {
    let cleaned = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.chars().count() <= max {
        return cleaned;
    }
    let cut: String = cleaned.chars().take(max).collect();
    format!("{cut}…")
}

/// 这个会话现在需要摘要吗？返回本次要折叠的轮次（可能为空）。
///
/// 只在「老于窗口的轮次还没被摘要覆盖过」时才给东西 —— 这是滚动语义的
/// 全部：`covered` 是水位线，只看水位线以上的新挤出部分。
pub fn newly_evicted(
    pairs: &[(String, String)],
    window: usize,
    covered: i32,
) -> Vec<(String, String)> {
    let evicted_len = pairs.len().saturating_sub(window);
    let covered = covered.max(0) as usize;
    if evicted_len <= covered {
        return Vec::new();
    }
    pairs[covered..evicted_len].to_vec()
}

/// 预算里装得下多少轮：从**最新**往回装，装到装不下为止。
///
/// 这是「按 token 触发压缩」的落点：预算小了，保留下来的轮次就少，
/// 被挤出去的部分由 `roll_forward` 折进摘要。至少保留 1 轮 —— 连最新一轮
/// 都不给的话，模型会对着摘要回答「你刚才说了什么」。
///
/// 估算是近似的（见 `joyczl_provider::tokens`），所以这里只需要量级对。
pub fn turns_that_fit(pairs: &[(String, String)], budget: usize) -> usize {
    let mut used = 0usize;
    let mut kept = 0usize;
    for (user, assistant) in pairs.iter().rev() {
        // 两条消息 + 各自固定开销（与 joyczl_provider::tokens 的算法保持一致）。
        let cost = joyczl_provider::tokens::estimate_text(user)
            + joyczl_provider::tokens::estimate_text(assistant)
            + 8;
        if kept > 0 && used + cost > budget {
            break;
        }
        used += cost;
        kept += 1;
    }
    kept
}

/// 把摘要接到 system prompt 里。空摘要返回 None，调用方就不加这一段。
pub fn summary_section(summary: &str) -> Option<String> {
    let summary = summary.trim();
    if summary.is_empty() {
        return None;
    }
    Some(format!("\nEarlier in this conversation:\n{summary}"))
}

/// 便捷入口：算 + 存 + 返回要用的摘要。`Ok(None)` 表示这次不需要动。
pub async fn refresh(
    chat: &joyczl_state::Chat,
    client: &dyn Provider,
    model: &str,
    session_id: &str,
    pairs: &[(String, String)],
    window: usize,
) -> Result<Option<String>> {
    let previous = chat.load_rollup(session_id).await.unwrap_or(None);
    let covered = previous.as_ref().map(|(c, _)| *c).unwrap_or(0);
    let fresh = newly_evicted(pairs, window, covered);
    if fresh.is_empty() {
        return Ok(previous.map(|(_, summary)| summary));
    }
    let (covered_now, summary) = roll_forward(client, model, previous, &fresh).await;
    // 存失败只影响下次重算（多花一次调用）—— 不能连累这轮对话。
    if let Err(e) = chat.save_rollup(session_id, covered_now, &summary).await {
        eprintln!("(joy) 滚动摘要落库失败（这轮照常用）：{e}");
    }
    Ok(Some(summary))
}
