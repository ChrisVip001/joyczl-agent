//! The triage graph workflow — the front door of every message when the flag is on.
//!
//! The retrieval gate asked a very narrow question with a small model ("does this
//! message need memory?"). Triage generalizes that idea from one gate into a
//! structure: two things happen **at once** (the small model classifies; today's
//! calendar is read from disk), then a code router picks the path:
//!
//! ```text
//! START ── classify ──────┐
//!      └── check_calendar ┴─► route:  quick → quick_reply (small model) → END
//!                                     full  → full_agent (THE loop) → END
//! ```
//!
//! "Thanks!" never wakes the big model. "Plan a swim on Saturday" runs the exact
//! same loop as always — just as a node in the graph. The user picks no mode;
//! this graph **is** the choice. And every seam fails open: classifier broken,
//! engine broken, any situation → answered by the plain loop, exactly as when
//! the flag is off.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use chrono::Local;
use joyczl_provider::{CreateRequest, Message, Provider};
use serde_json::json;

use crate::nodes::fn_node;
use crate::workflow_def::{builtin_routes, WorkflowDef};
use crate::{writes, Boxed, Graph, NodeCtx, NodeFn, NodeWrites, State};

/// 拓扑的唯一定义，编译期内嵌（`definitions/triage.json`）。工作流是代码的一部分，
/// 随二进制发布——改动走 review，不走运行时加载。
const TRIAGE_DEF: &str = include_str!("definitions/triage.json");

pub const TRIAGE_PROMPT: &str = r#"You are a triage gate for a personal assistant. Given the user's message,
decide which brain should answer it.

Reply with ONLY this JSON, nothing else:
{"route": "quick" or "full", "reason": "<5 words>"}

quick — greetings, thanks, acknowledgements, pure small talk: nothing to do,
        nothing to look up.
full  — anything mentioning tasks, schedules, people, notes, memory, or that
        needs a tool. When unsure, choose full.

User message: {message}"#;

pub const QUICK_REPLY_PROMPT: &str = r#"You are Joy, a warm, concise personal assistant. The user's message needs no
tools or memory — reply in one or two short, natural sentences. If today's
calendar (below) is clearly relevant, you may mention it; otherwise ignore it.

Today's calendar: {calendar}

User message: {message}"#;

/// The four injected callables: tests script them, the dashboard builds a stubbed
/// graph just for describe(), and app-server binds the real client / loop.
pub type ClassifyFn = Arc<dyn Fn(String) -> Boxed<(String, String)> + Send + Sync>;
pub type CalendarFn = Arc<dyn Fn() -> String + Send + Sync>;
pub type QuickFn = Arc<dyn Fn(State) -> Boxed<String> + Send + Sync>;
pub type FullFn = Arc<dyn Fn(NodeCtx) -> Boxed<NodeWrites> + Send + Sync>;

/// Returns (route, reason). Fails open to "full": a broken triage must cost latency,
/// never capability — the same mirror rule as `retrieval_gate`.
pub async fn classify_message(
    client: &dyn Provider,
    small_model: &str,
    message: &str,
) -> (String, String) {
    let request = CreateRequest {
        cache_prefix: None,
        model: small_model.to_string(),
        system: None,
        messages: vec![Message::user_text(
            TRIAGE_PROMPT.replace("{message}", message),
        )],
        tools: Vec::new(),
        // A reasoning model needs a moment to think before emitting that JSON.
        max_tokens: 600,
    };
    let response = match client.create(request).await {
        Ok(response) => response,
        Err(_) => {
            return (
                "full".to_string(),
                "triage failed open (ProviderError)".to_string(),
            )
        }
    };
    let text = response.text();
    let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) else {
        return ("full".to_string(), "no JSON — failing open".to_string());
    };
    if end < start {
        return ("full".to_string(), "no JSON — failing open".to_string());
    }
    let decision: serde_json::Value = match serde_json::from_str(&text[start..=end]) {
        Ok(decision) => decision,
        Err(_) => {
            return (
                "full".to_string(),
                "triage failed open (ParseError)".to_string(),
            )
        }
    };
    let route = decision.get("route").and_then(|r| r.as_str()).unwrap_or("");
    if route != "quick" && route != "full" {
        return (
            "full".to_string(),
            format!("bad route '{route}' — failing open"),
        );
    }
    (
        route.to_string(),
        decision
            .get("reason")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string(),
    )
}

/// Today's events, read straight from `<home>/calendar.ics` — one local file read,
/// which is exactly why it can run alongside the classifier's network call.
pub fn todays_events(home: &Path) -> String {
    let ics = home.join("calendar.ics");
    let Ok(text) = std::fs::read_to_string(&ics) else {
        return "(no calendar)".to_string();
    };
    let today = Local::now().format("%Y%m%d").to_string();
    let mut lines: Vec<String> = Vec::new();
    let mut title = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("SUMMARY:") {
            title = rest.to_string();
        } else if line.starts_with("DTSTART") && line.contains(&today) && !title.is_empty() {
            lines.push(title.clone());
        }
    }
    if lines.is_empty() {
        "(nothing today)".to_string()
    } else {
        lines.join("; ")
    }
}

pub fn build_triage_graph(
    classify_fn: ClassifyFn,
    calendar_fn: CalendarFn,
    quick_fn: QuickFn,
    full_fn: FullFn,
) -> Result<Graph, String> {
    // 拓扑在 definitions/triage.json 里（编译期内嵌，启动即校验）；这里只按节点名
    // 注入行为 —— 与引擎的约定一致：JSON 声明形状，代码提供动作。
    let mut bindings: BTreeMap<String, NodeFn> = BTreeMap::new();

    let classify = classify_fn.clone();
    bindings.insert(
        "classify".to_string(),
        Arc::new(move |ctx: NodeCtx| {
            let classify = classify.clone();
            let message = ctx.state.str("message").unwrap_or_default().to_string();
            Box::pin(async move {
                let (route, reason) = classify(message).await?;
                Ok(writes([
                    ("route", json!(route)),
                    ("triage_reason", json!(reason)),
                ]))
            })
        }),
    );

    bindings.insert(
        "check_calendar".to_string(),
        fn_node(move |_| writes([("calendar", json!(calendar_fn()))])),
    );

    bindings.insert(
        "quick_reply".to_string(),
        Arc::new(move |ctx: NodeCtx| {
            let quick = quick_fn.clone();
            let state = ctx.state;
            Box::pin(async move {
                let reply = quick(state).await?;
                Ok(writes([("reply", json!(reply))]))
            })
        }),
    );

    // This node is "an entire loop". It calls the same full_turn as without a graph.
    bindings.insert(
        "full_agent".to_string(),
        Arc::new(move |ctx: NodeCtx| full_fn(ctx)),
    );

    // Does nothing but wait for **both** parallel branches to arrive — then the router decides.
    bindings.insert("gather".to_string(), fn_node(|_| NodeWrites::new()));

    WorkflowDef::parse(TRIAGE_DEF)?.build(&bindings, &builtin_routes())
}
