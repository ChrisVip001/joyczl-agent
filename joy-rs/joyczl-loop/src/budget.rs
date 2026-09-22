//! 轮内工具结果预算：**组装请求前，把太大的结果换成桩**。
//!
//! 为什么需要它：`run_command` 自己会把输出截到 8000 字符、`search_web` 自己截到
//! 400，但 **MCP 工具的输出没人管**。一轮里十次 MCP 调用各回 5 万字符，就是
//! 50 万字符进请求 —— 而 app-server 的预算只管历史（滑窗），管不到轮内。
//!
//! 顺序照 deepseek-harness / hermes 的一致结论：**先处理工具结果，再谈摘要**。
//! 工具结果有三个别人没有的性质：能落盘回查、能重跑、裁剪不需要模型调用。
//!
//! 三条规矩：
//!
//! * **够大才动**：只处理超过 `per_result_chars` 的结果。攒了一百条小结果导致
//!   总量超标时，给每条都建一个文件比省下的上下文更贵 —— 那种情况如实记一行
//!   日志，不改内容。
//! * **不重复处理**：已经是桩的（带「完整输出在」或「已截断」）直接跳过，
//!   所以每轮跑一次是幂等的。
//! * **自预算的工具不碰**：`run_command` 自己就落盘了，再套一层只会把路径
//!   换来换去（它的输出永远 ≤ 8000，够不到 `per_result_chars`，这条是保险）。

use std::collections::HashMap;
use std::path::Path;

use joyczl_provider::{ContentBlock, Message};

/// 自己能把自己管住的工具：不参与轮内换桩。
const SELF_BUDGETED: &[&str] = &["run_command"];

/// 轮内工具结果的预算。两个都为 0 = 关（默认关，与「不改变既有行为」一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolResultBudget {
    /// 一批工具结果的字符总量上限。
    pub total_chars: usize,
    /// 单条结果超过它才有资格被换成桩。
    pub per_result_chars: usize,
}

impl Default for ToolResultBudget {
    /// 默认**关着**：不改变既有行为，要开就配 `JOY_TOOL_RESULT_TOTAL_CHARS`。
    fn default() -> Self {
        Self::disabled()
    }
}

impl ToolResultBudget {
    pub fn disabled() -> Self {
        Self {
            total_chars: 0,
            per_result_chars: 0,
        }
    }

    pub fn enabled(&self) -> bool {
        self.total_chars > 0 && self.per_result_chars > 0
    }
}

/// 一次换桩的结果（给日志与测试看）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TrimReport {
    /// 换成桩的结果条数。
    pub trimmed: usize,
    /// 换桩后省下的字符数。
    pub omitted_chars: usize,
    /// 换完之后这批结果的总字符数。
    pub total_chars: usize,
    /// 该换但换不动（剩下的都不够大）：如实记下来，不假装已经达标。
    pub still_over_budget: bool,
}

/// 组装请求前调一次：把超预算的工具结果换成「放得下的桩」。
///
/// 返回这次动了什么。预算关着时什么都不做（默认就是关着）。
pub fn trim_tool_results(
    messages: &mut [Message],
    spill_dir: Option<&Path>,
    budget: &ToolResultBudget,
    session_id: &str,
) -> TrimReport {
    if !budget.enabled() {
        return TrimReport::default();
    }

    let names = tool_names(messages);
    let blocks: Vec<(usize, usize)> = collect_results(messages);
    let total_chars: usize = blocks
        .iter()
        .map(|(message, block)| result_len(messages, *message, *block))
        .sum();
    let mut report = TrimReport {
        total_chars,
        ..TrimReport::default()
    };
    if report.total_chars <= budget.total_chars {
        return report;
    }

    // 从最大的开始换 —— 换最少条数就能达标。
    let mut candidates: Vec<(usize, usize, usize, Option<&str>)> = blocks
        .iter()
        .map(|(message, block)| {
            let len = result_len(messages, *message, *block);
            let name = name_of(messages, *message, *block, &names);
            (*message, *block, len, name)
        })
        .filter(|(message, block, len, name)| {
            *len > budget.per_result_chars
                && !is_stub(result_text(messages, *message, *block))
                && !name.is_some_and(|name| SELF_BUDGETED.contains(&name))
        })
        .collect();
    candidates.sort_by_key(|(_, _, len, _)| std::cmp::Reverse(*len));

    for (message, block, len, _) in candidates {
        if report.total_chars <= budget.total_chars {
            break;
        }
        let text = result_text(messages, message, block).to_string();
        let Some(stub) =
            joyczl_tools::spill::stub(spill_dir, &text, stub_budget(budget), "tool-result")
        else {
            continue;
        };
        let saved = text
            .chars()
            .count()
            .saturating_sub(stub.text.chars().count());
        let path = stub.path.clone();
        set_result_text(messages, message, block, stub.text);
        report.trimmed += 1;
        report.omitted_chars += saved;
        report.total_chars = report.total_chars.saturating_sub(saved);
        eprintln!(
            "(joy) [{session_id}] 轮内工具结果超预算，已换成桩：省下 {saved} 字符（共 {} 条）{}",
            report.trimmed,
            match path {
                Some(path) => format!("，完整输出在 {path}"),
                None => "（落盘失败，桩里没有可回查的路径）".to_string(),
            }
        );
        let _ = len;
    }

    report.still_over_budget = report.total_chars > budget.total_chars;
    if report.still_over_budget {
        // 不静默：够大的都换完了还超，说明是「很多条中等结果」堆出来的 ——
        // 给每条都建文件比省下的上下文更贵，所以到此为止。
        eprintln!(
            "(joy) [{session_id}] 轮内工具结果仍然超出预算（{} > {}）：剩下的单条都没超过 {} 字符",
            report.total_chars, budget.total_chars, budget.per_result_chars
        );
    }
    report
}

/// 桩的目标大小：比门槛小一档就够（它的用途是把结果请出上下文）。
///
/// **下限必须远小于门槛**：门槛配得很小（评测就爱这么干）时，若下限比门槛还大，
/// 就会出现「够格换桩、却因为桩放不下原文而换不掉」的哑火 —— 那是真踩过的。
/// 上限 2000 抄的是 deepseek-harness 的预览量级。
fn stub_budget(budget: &ToolResultBudget) -> usize {
    budget.per_result_chars.clamp(64, 2_000)
}

/// 已经是桩了（幂等：每轮都跑一次）。
fn is_stub(text: &str) -> bool {
    text.contains("完整输出在") || text.contains("已截断，省略了")
}

fn collect_results(messages: &[Message]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (message_index, message) in messages.iter().enumerate() {
        for (block_index, block) in message.content.iter().enumerate() {
            if matches!(block, ContentBlock::ToolResult { .. }) {
                out.push((message_index, block_index));
            }
        }
    }
    out
}

fn result_len(messages: &[Message], message: usize, block: usize) -> usize {
    result_text(messages, message, block).chars().count()
}

fn result_text(messages: &[Message], message: usize, block: usize) -> &str {
    match &messages[message].content[block] {
        ContentBlock::ToolResult { content, .. } => content,
        _ => "",
    }
}

fn set_result_text(messages: &mut [Message], message: usize, block: usize, text: String) {
    if let ContentBlock::ToolResult { content, .. } = &mut messages[message].content[block] {
        *content = text;
    }
}

/// `tool_use_id → 工具名`。工具结果本身不带名字，得从 assistant 的调用里查。
fn tool_names(messages: &[Message]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for message in messages {
        for block in &message.content {
            if let ContentBlock::ToolUse { id, name, .. } = block {
                names.insert(id.clone(), name.clone());
            }
        }
    }
    names
}

fn name_of<'a>(
    messages: &[Message],
    message: usize,
    block: usize,
    names: &'a HashMap<String, String>,
) -> Option<&'a str> {
    let id = match &messages[message].content[block] {
        ContentBlock::ToolResult { tool_use_id, .. } => tool_use_id,
        _ => return None,
    };
    names.get(id).map(String::as_str)
}
