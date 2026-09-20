//! triage 图工作流 —— flag 打开时，每句话的前门。
//!
//! 检索门用小模型问了一个很窄的问题（「这句话需要记忆吗」）。triage 把那
//! 个想法从一道门推广成一个结构：两件事**同时**发生（小模型给消息分类；
//! 今天的日历从磁盘读出来），然后由代码路由器挑路：
//!
//! ```text
//! START ── classify ──────┐
//!      └── check_calendar ┴─► route:  quick → quick_reply（小模型）→ END
//!                                     full  → full_agent（THE loop）→ END
//! ```
//!
//! 「谢谢！」永远不会吵醒大模型。「周六安排个游泳」跑的是跟平时一字不差
//! 的 loop —— 只是作为图里的一个节点。用户不选模式；这张图**就是**那个
//! 选择。而且每道缝都失败开放：分类器坏了、引擎坏了，任何情况 → 都由
//! 普通 loop 回答，跟 flag 关着时一样。

use std::path::Path;
use std::sync::Arc;

use chrono::Local;
use joyczl_provider::{CreateRequest, Message, Provider};
use serde_json::json;

use crate::nodes::{fn_node, key_router};
use crate::{writes, Boxed, Graph, GraphError, Node, NodeCtx, NodeWrites, State, Topology, END};

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

/// 注入的四个可调用体：测试能脚本化它们，dashboard 用桩建图只为 describe()，
/// app-server 则把真的 client / loop 绑上去。
pub type ClassifyFn = Arc<dyn Fn(String) -> Boxed<(String, String)> + Send + Sync>;
pub type CalendarFn = Arc<dyn Fn() -> String + Send + Sync>;
pub type QuickFn = Arc<dyn Fn(State) -> Boxed<String> + Send + Sync>;
pub type FullFn = Arc<dyn Fn(NodeCtx) -> Boxed<NodeWrites> + Send + Sync>;

/// 返回 (route, reason)。失败开放到 "full"：triage 坏掉的代价必须是延迟，
/// 绝不能是能力 —— 跟 `retrieval_gate` 同一条镜像规则。
pub async fn classify_message(
    client: &dyn Provider,
    small_model: &str,
    message: &str,
) -> (String, String) {
    let request = CreateRequest {
        model: small_model.to_string(),
        system: None,
        messages: vec![Message::user_text(
            TRIAGE_PROMPT.replace("{message}", message),
        )],
        tools: Vec::new(),
        // 会思考的模型得先想一会儿才吐出那段 JSON。
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

/// 今天的事件，直接从 `<home>/calendar.ics` 读 —— 一次本地文件读，
/// 这正是它能跟分类器那次网络调用并排跑的原因。
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
) -> Result<Graph, GraphError> {
    let mut g = Graph::new("triage");

    let classify = classify_fn.clone();
    g.add_node(Node::new(
        "classify",
        "llm",
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
    ))?;

    let calendar = calendar_fn.clone();
    g.add_node(Node::new(
        "check_calendar",
        "tool",
        fn_node(move |_| writes([("calendar", json!(calendar()))])),
    ))?;

    let quick = quick_fn.clone();
    g.add_node(Node::new(
        "quick_reply",
        "llm",
        Arc::new(move |ctx: NodeCtx| {
            let quick = quick.clone();
            let state = ctx.state;
            Box::pin(async move {
                let reply = quick(state).await?;
                Ok(writes([("reply", json!(reply))]))
            })
        }),
    ))?;

    // 这个节点就是「一整轮 loop」。它调的是不带图时同一个 full_turn。
    let full = full_fn.clone();
    g.add_node(Node::new(
        "full_agent",
        "agent",
        Arc::new(move |ctx: NodeCtx| full(ctx)),
    ))?;

    g.entry(&["classify", "check_calendar"])?;

    // 什么都不做，只等**两条**并行分支都到齐 —— 然后路由器才开始做决定。
    g.add_node(Node::new("gather", "fn", fn_node(|_| NodeWrites::new())))?;
    g.add_edge("classify", "gather")?;
    g.add_edge("check_calendar", "gather")?;
    g.add_router(
        "gather",
        key_router("route", "full"),
        &[("quick", "quick_reply"), ("full", "full_agent")],
    )?;
    g.add_edge("quick_reply", END)?;
    g.add_edge("full_agent", END)?;

    Ok(g)
}

/// 拓扑即数据，给 dashboard 用 —— 用桩建图，从不运行。
pub fn triage_topology() -> Topology {
    let noop_classify: ClassifyFn =
        Arc::new(|_m| Box::pin(async { Ok(("full".to_string(), String::new())) }));
    let noop_calendar: CalendarFn = Arc::new(String::new);
    let noop_quick: QuickFn = Arc::new(|_s| Box::pin(async { Ok(String::new()) }));
    let noop_full: FullFn = Arc::new(|_c| Box::pin(async { Ok(NodeWrites::new()) }));
    match build_triage_graph(noop_classify, noop_calendar, noop_quick, noop_full) {
        Ok(graph) => graph.describe(),
        // 走到这里说明图本身写错了，而那张图是写死在这个文件里的。
        Err(error) => panic!("triage 图写错了：{error}"),
    }
}
