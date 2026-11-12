//! Intra-turn tool-result budget: **stub out oversized results before assembling the request**.
//!
//! Why it is needed: `run_command` truncates its own output to 8000 characters and `search_web`
//! to 400, but **nobody watches MCP tool output**. Ten MCP calls per turn each returning 50k
//! characters put 500k characters into the request — and the app-server budget only covers history.
//!
//! The agreed conclusion on ordering: **handle tool results first, then talk summarization**.
//! Tool results alone can be spilled for later lookup, re-run, and trimmed without a model call.
//!
//! Four rules:
//!
//! * **Only touch the big ones**: only results over `per_result_chars`. When a hundred small
//!   results together exceed the cap, a file per result costs more than the context it saves —
//!   in that case just log one honest line and change nothing.
//! * **Oldest first, newest kept**: the model is *working* with the last few results; stubbing
//!   one of those answers a question it just asked. Ordering by size (the old rule) could gut
//!   the freshest result while an old settled one stayed whole. Age is the honest axis.
//! * **No reprocessing**: anything already a stub (carrying the spill marker or the truncation
//!   marker, as matched by `is_stub`) is skipped, so running once per turn is idempotent.
//! * **Self-budgeted tools untouched**: `run_command` already spills itself; another wrapper
//!   just swaps paths around (its output is always ≤ 8000, below `per_result_chars`; safety net).

use std::collections::HashMap;
use std::path::Path;

use joyczl_provider::{ContentBlock, Message};

/// Tools that manage themselves: they do not participate in intra-turn stubbing.
const SELF_BUDGETED: &[&str] = &["run_command"];

/// Is this turn's token budget spent enough that the **next** round should be the last one?
///
/// `spent` is what the provider reported so far (input + output), `next_input` the last request's
/// own input estimate — the next call will cost about that again, so counting it is what keeps
/// "80%" from meaning "we stop at 95%".
///
/// **One threshold**, at 80%. Hitting 80% and hitting 100% get the same response (tools away, one
/// wrap-up call), so a second threshold would be a second name for the same branch; 80% exists to
/// leave room for that wrap-up call, which is itself a request.
///
/// `None` and 0 both mean off — the step cap already bounds rounds, and a cap nobody asked for
/// cuts honest work short.
pub fn should_wrap_up(budget: Option<usize>, spent: usize, next_input: usize) -> bool {
    let Some(budget) = budget.filter(|budget| *budget > 0) else {
        return false;
    };
    let horizon = budget.saturating_mul(4) / 5;
    spent.saturating_add(next_input) >= horizon
}

/// Budget for intra-turn tool results. Both 0 = off (off by default, matching "no behavior change").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolResultBudget {
    /// Char limit for a batch of tool results.
    pub total_chars: usize,
    /// A result must exceed this to be eligible for stubbing.
    pub per_result_chars: usize,
    /// How many of the newest results are never stubbed (`JOY_TOOL_RESULT_KEEP_RECENT`,
    /// default 3). Recent results are what the model is reasoning about right now.
    pub keep_recent: usize,
}

impl Default for ToolResultBudget {
    /// Defaults to **off**: no behavior change; to enable set `JOY_TOOL_RESULT_TOTAL_CHARS`.
    fn default() -> Self {
        Self::disabled()
    }
}

impl ToolResultBudget {
    pub fn disabled() -> Self {
        Self {
            total_chars: 0,
            per_result_chars: 0,
            keep_recent: 0,
        }
    }

    pub fn enabled(&self) -> bool {
        self.total_chars > 0 && self.per_result_chars > 0
    }
}

/// Result of one stubbing pass (for logging and tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TrimReport {
    /// Number of results stubbed out.
    pub trimmed: usize,
    /// Characters saved by stubbing.
    pub omitted_chars: usize,
    /// Total chars of this batch after stubbing.
    pub total_chars: usize,
    /// Over budget but nothing left worth stubbing: recorded honestly, not faked as done.
    pub still_over_budget: bool,
}

/// Call once before assembling the request: stub tool results that are over budget.
///
/// Returns what changed this pass. Does nothing when the budget is off (the default).
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

    // Oldest first; the newest `keep_recent` results are off limits. `collect_results`
    // already walks messages in order, so the position in that vector *is* the age order.
    let keep = budget.keep_recent.min(blocks.len());
    let stubbable = blocks.len() - keep;
    let candidates: Vec<(usize, usize, usize, Option<&str>)> = blocks
        .iter()
        .take(stubbable)
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
        // Not silent: when everything big enough is stubbed and we are still over, this is a pile
        // of medium-sized results — a file per result costs more than the context saved, so stop.
        eprintln!(
            "(joy) [{session_id}] 轮内工具结果仍然超出预算（{} > {}）：剩下的单条都没超过 {} 字符{}",
            report.total_chars,
            budget.total_chars,
            budget.per_result_chars,
            if keep > 0 {
                format!("，最新的 {keep} 条按策略保留")
            } else {
                String::new()
            }
        );
    }
    report
}

/// Target stub size: one notch below the threshold is enough (its job is to evict the result).
///
/// **The floor must be far below the threshold**: with a tiny threshold (evaluations love that),
/// a floor at or above the threshold causes a dud — "eligible for stubbing, but the stub cannot
/// hold the original" — a real failure seen in practice. The 2000 cap matches head/tail previews.
fn stub_budget(budget: &ToolResultBudget) -> usize {
    budget.per_result_chars.clamp(64, 2_000)
}

/// Already a stub (idempotent: runs once per turn).
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

/// `tool_use_id → tool name`. Results carry no name; look it up in the assistant's calls.
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
