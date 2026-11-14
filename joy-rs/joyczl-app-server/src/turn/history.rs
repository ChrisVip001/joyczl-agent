//! History stage of the turn lifecycle.

use std::sync::Arc;

use crate::Server;
use joyczl_protocol::{GoalRoundNotification, ServerNotification};
use joyczl_provider::{ContentBlock, Message, Resolved, Role};

/// One full loop: retrieval gate → system prompt → sliding-window history → THE LOOP.
///
/// When the graph's front door is open, the `full_agent` node calls this function — the default path
/// without the graph and the path inside the graph **share the same implementation**, so the
/// "loop as node" can never drift from the "loop as default".
///
/// `inner` is the event outlet the node leaves for the loop: when running inside the graph, events must
/// leave through it so the engine can attach `node=full_agent`. `None` when there is no graph.
///
/// Turns force-kept on overflow retry: keep the newest two turns verbatim, fold the rest into the summary.
pub(super) const FORCED_RETRY_TURNS: usize = 2;

/// Reserve for the summary section: the summary model's cap (700 tokens) + section header and slack.
/// The summary is generated inside load_history; at budget time it does not exist yet, so it must be set aside up front.
pub(super) const SUMMARY_RESERVE_TOKENS: usize = 800;

/// This turn's context budget (tokens): window (`JOY_CONTEXT_WINDOW` first, else the
/// provider table's approximation) × ratio. Floor of 1024 — however small the ratio, never squeeze
/// the window below fitting even one turn of conversation.
///
/// `factor` is last turn's measured/estimated ratio (see `session_context`): **calibrate estimates with measurements**.
/// When the estimate is too small (factor > 1), shrink the budget so compaction happens sooner; relax when it is too large.
/// No record (first turn, or the graph path) means no calibration.
pub(super) fn budget_tokens(
    resolved: &Resolved,
    settings: &joyczl_config::Settings,
    factor: Option<f64>,
) -> usize {
    let window = settings
        .context_window
        .unwrap_or_else(|| resolved.context_window());
    let raw = window as f64 * settings.compact_threshold;
    let calibrated = match factor {
        Some(factor) if factor > 0.0 => raw / factor,
        _ => raw,
    };
    (calibrated as usize).max(1024)
}

/// Is this error a "context overflow"? The loop wrapped the provider error in anyhow;
/// drill down one level here to recognize it — overflow is the only error worth "retry once after compaction".
pub(super) fn is_context_overflow(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<joyczl_provider::ProviderError>()
        .is_some_and(joyczl_provider::ProviderError::is_context_overflow)
}

/// The transcript for the judge: the session's recent turns, with this turn's reply appended.
///
/// The judge **only reads the conversation** (no tools, no retrieval): what it must answer is "does this
/// conversation show the goal is met"; looking at anything else only makes it start doing the model's work.
pub(super) async fn judge_transcript(server: &Server, session_id: &str, reply: &str) -> String {
    let history = server
        .chat
        .session_history(session_id)
        .await
        .unwrap_or_default();
    if history.is_empty() {
        return format!("assistant: {reply}");
    }
    let mut out = String::new();
    let recent = history.len().saturating_sub(8);
    for (role, content) in &history[recent..] {
        out.push_str(&format!("{role}: {content}\n"));
    }
    out.push_str(&format!("assistant: {reply}"));
    out
}

/// Emit one goal-loop notification (`GoalRound` is the only answer to "why another round ran").
pub(super) fn notify_goal(
    sink: &crate::EventSink,
    turn_id: &str,
    round: i32,
    max_rounds: i32,
    status: &str,
    reason: &str,
) {
    sink.notification(ServerNotification::GoalRound(GoalRoundNotification {
        turn_id: turn_id.to_string(),
        round,
        max_rounds,
        status: status.to_string(),
        reason: reason.to_string(),
    }));
}

/// Is there work this turn itself declared unfinished?
///
/// The todo list is the model's **own** written statement of what is left, so it is the one
/// "are you done?" signal that costs no model call and cannot be talked around. Three rules keep
/// it from becoming a loop driver:
///
/// * `interrupted` means the human said "enough" — that is not unfinished work;
/// * no tool call this turn means a conversation, and a stale board from an earlier turn must not
///   drag a chat turn back to work it never reopened;
/// * a goal-carrying turn is driven by the goal loop instead (checked by the caller).
pub(super) fn unfinished_work(
    server: &Server,
    session_id: &str,
    result: &joyczl_loop::LoopResult,
) -> Option<String> {
    if result.interrupted || result.tool_calls.is_empty() {
        return None;
    }
    let list = server.todo.read(session_id)?;
    let open: Vec<&joyczl_tools::todo::TodoItem> = list
        .items
        .iter()
        .filter(|item| {
            matches!(
                item.status,
                joyczl_tools::todo::TodoStatus::Pending
                    | joyczl_tools::todo::TodoStatus::InProgress
            )
        })
        .collect();
    if open.is_empty() {
        return None;
    }
    let mut listed: Vec<String> = open
        .iter()
        .take(3)
        .map(|item| format!("{} ({})", item.content, item.status.as_str()))
        .collect();
    if open.len() > listed.len() {
        listed.push(format!("and {} more", open.len() - listed.len()));
    }
    Some(format!(
        "{} to-do items are still open: {}",
        open.len(),
        listed.join("；")
    ))
}

/// Take only the most recent N turns, plus a rolling summary.
///
/// Without a sliding window, a long session stuffs the entire history into the prompt every turn until
/// the context explodes; with only a sliding window, the squeezed-out part equals amnesia. So: the squeezed-out
/// What one turn's history (and its compaction) looked like — the numbers `traces/*.jsonl` wants.
///
/// All of them are "before/after" in **turns**, not tokens: what compaction actually decides is
/// how many turns survive, and the token estimate that drove that decision is already recorded
/// on the result (`estimatedInputTokens` / `observedInputTokens`).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct HistoryStats {
    pub(crate) turns_total: usize,
    pub(crate) turns_kept: usize,
    pub(crate) compacted: bool,
}

/// The result of loading history: what goes into the prompt, the summary, and what it cost.
pub(crate) struct Loaded {
    pub(crate) messages: Vec<Message>,
    pub(crate) summary: Option<String>,
    pub(crate) stats: HistoryStats,
}

impl Loaded {
    /// No history (first turn / resume / compaction failed). Stats are zero, which is honest.
    pub(super) fn empty() -> Self {
        Self {
            messages: Vec::new(),
            summary: None,
            stats: HistoryStats::default(),
        }
    }
}

/// old turns are handed to `compaction`, folded into one summary (fail-open; worst case a truncated excerpt),
/// stored with the session, and spliced into the system prompt every turn. Returns `(working memory, summary)`.
/// `token_budget` is the approximate token count this turn **may spend on history**; `window_override`
/// is the "force compaction after context overflow" path: it pins how many turns to keep, ignoring the budget.
#[allow(clippy::too_many_arguments)]
pub(super) async fn load_history(
    chat: &joyczl_state::Chat,
    resolved: &Resolved,
    session_id: &str,
    history_turns: i32,
    token_budget: usize,
    window_override: Option<usize>,
    hooks: &Option<Arc<joyczl_tools::hooks::Hooks>>,
) -> Loaded {
    let pairs = chat.session_history(session_id).await.unwrap_or_default();
    let ceiling = history_turns.max(0) as usize;
    // The turn count is the ceiling; tokens are the gate.
    let window = match window_override {
        Some(forced) => forced,
        None => ceiling.min(joyczl_memory::compaction::turns_that_fit(
            &pairs,
            token_budget,
        )),
    };

    // PreCompact must fire **before acting** (a note after the fact is not "Pre"). Whether compaction will
    // actually happen is decided in one place, `compaction::due` — the hook should not ring when nothing was really compacted.
    let will_compact = joyczl_memory::compaction::due(chat, session_id, &pairs, window).await;
    if will_compact {
        let outcome = crate::fire_hook(
            hooks,
            joyczl_tools::hooks::HookEvent::PreCompact,
            serde_json::json!({ "session_id": session_id, "window": window, "turns": pairs.len() }),
        )
        .await;
        // Observe-only event: the hook's words are a side note and must not block compaction (blocking it could overflow this turn).
        if let Some(note) = outcome.note {
            eprintln!("(joy) PreCompact：{note}");
        }
    }

    let summary = joyczl_memory::compaction::refresh(
        chat,
        resolved.client.as_ref(),
        &resolved.small_model,
        session_id,
        &pairs,
        window,
    )
    .await
    .unwrap_or(None);

    if will_compact {
        crate::fire_hook(
            hooks,
            joyczl_tools::hooks::HookEvent::PostCompact,
            serde_json::json!({
                "session_id": session_id,
                "summary_chars": summary.as_ref().map(String::len).unwrap_or(0),
            }),
        )
        .await;
    }

    let messages = pairs
        .iter()
        .rev()
        .take(window)
        .rev()
        .flat_map(|(user, assistant)| {
            vec![
                Message::user_text(user.clone()),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: assistant.clone(),
                    }],
                },
            ]
        })
        .collect();
    Loaded {
        messages,
        summary,
        stats: HistoryStats {
            turns_total: pairs.len(),
            turns_kept: window,
            compacted: will_compact,
        },
    }
}

/// Fold what the tools did into the assistant text that gets written to `chat_log`.
///
/// This is the **only** trace of a tool call that survives into the next turn's history (the
/// conversation is text-level; tool_use/tool_result blocks are rebuilt per turn). So the line
/// has to carry three things: what ran, how much came back, and — when the output was spilled
/// to disk — **where to read it again**. Without the address a long task re-reads the same
/// files every turn, because the model can see it learned something but not what.
pub(crate) fn fold_tool_activity(reply: &str, calls: &[joyczl_loop::ToolOutcome]) -> String {
    if calls.is_empty() {
        return reply.to_string();
    }
    let summary = calls.iter().map(fold_one).collect::<Vec<_>>().join("; ");
    format!("{reply}\n[tools used: {summary}]")
}

/// One call, one line: `name(1.2k chars, full output at spill/20261112/exec-….txt: first line…)`.
pub(super) fn fold_one(call: &joyczl_loop::ToolOutcome) -> String {
    let chars = call.output.chars().count();
    let first = call.output.lines().next().unwrap_or("").trim();
    let head = truncate(first, 160);
    match spill_address(&call.output) {
        Some(address) => format!(
            "{}({} chars, full output at {address}; first line: {})",
            call.name, chars, head
        ),
        None => format!("{}({} chars: {})", call.name, chars, head),
    }
}

/// The spill address inside a tool result, if it carries one.
///
/// Parsed back out of the text instead of threaded through the loop: the text the model read
/// is exactly the text folded here, so there is one source of truth and no second field to
/// keep in sync (`joyczl_tools::spill` writes "... full output at <path>)").
pub(super) fn spill_address(output: &str) -> Option<&str> {
    /// The Chinese markers are still here on purpose: ledgers written before the wording was
    /// translated contain them, and a resume re-reads those rows.
    const MARKERS: &[&str] = &["完整输出在 ", "全量输出在 ", "full output at "];
    for marker in MARKERS {
        let Some(start) = output.rfind(marker) else {
            continue;
        };
        let rest = &output[start + marker.len()..];
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '）' || c == ')' || c == '，' || c == '。')
            .unwrap_or(rest.len());
        let address = rest[..end].trim();
        if !address.is_empty() {
            return Some(address);
        }
    }
    None
}

pub(super) fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{head}…")
    }
}
