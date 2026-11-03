//! The gather graph workflow — the morning report, the one that truly earns "graph".
//!
//! Every morning asks the same four questions: what's open in the repo, what's being
//! said out there, what's scheduled today, what do I already know. The list isn't
//! discovered — it's known before waking up. Feeling around tool by tool buys nothing.
//!
//! ```text
//! START ─┬─ scan_github  ─┐
//!        ├─ scan_web      │
//!        ├─ scan_calendar ├─► synthesize ──router──► draft_digest → END
//!        └─ scan_memory  ─┘                    └──── quiet ──────► END
//! ```
//!
//! The four scans don't depend on each other; the engine runs them in one parallel wave —
//! the whole argument for a graph here: the shape is known, so it can be drawn.
//!
//! Two iron rules this file obeys (both load-bearing):
//!
//! 1. **Propose only, never act.** No agent node, no loop, no ToolRegistry in the
//!    graph. The only model call is a bare `create`, **with no tools parameter** —
//!    the model holds no schema for sending, merging, or creating anything. Not
//!    "told not to" but "unable to". The graph's only write is a markdown file
//!    in the outbox, for a human to read.
//!
//! 2. **Each branch contains its own failure.** A node that errors fires no edges,
//!    so synthesize's dependencies never complete and the whole run yields nothing —
//!    no error, no report, an empty outbox. For a morning report, silence is the
//!    worst failure mode, so every scan returns **honest text**
//!    ("unavailable (…)") instead of an error. See each node's safe wrapper below.

use std::sync::Arc;

use joyczl_provider::{CreateRequest, Message};
use serde_json::json;

use crate::{writes, Boxed, Graph, GraphError, Node, NodeCtx, NodeWrites, State, Topology, END};

/// Each scan only writes keys carrying its own prefix. Parallel nodes writing the same
/// key trips GraphStateCollision — the backstop; the reviewer-checkable convention is the defense.
pub const DIGEST_PROMPT: &str = r#"You are preparing a morning briefing for the maintainer of an
open-source project. Below is everything gathered a moment ago.

OPEN PULL REQUESTS AND ISSUES:
{gh_text}

WHAT THE WEB SAYS:
{web_text}

TODAY'S CALENDAR:
{cal_text}

WHAT YOU ALREADY KNOW:
{mem_text}

Write a short briefing in markdown:
- Lead with the two or three things that actually matter today, and say why.
- Then anything waiting on the maintainer, grouped, one line each.
- End with a single suggested focus for the day.

You are DRAFTING A PROPOSAL for a human to act on. You have not done anything
and cannot do anything — never write as if you have replied, merged, or sent.
Be brief. If a section gathered nothing, say so in a few words and move on."#;

// ---- Injected callables ------------------------------------------------------
//
// Same approach as triage: tests use scripted stubs, gather_topology() builds a
// stubbed graph just for describe(), the CLI binds real implementations. Scans are
// async (real ones spawn subprocesses and HTTP); `Boxed<T>`'s Err(String) is the
// failure reason — nodes translate it into honest text, see `unavailable`.

pub type GithubScanFn = Arc<dyn Fn() -> Boxed<(String, i64, i64)> + Send + Sync>;
pub type WebScanFn = Arc<dyn Fn() -> Boxed<String> + Send + Sync>;
pub type CalendarScanFn = Arc<dyn Fn() -> Boxed<(String, i64)> + Send + Sync>;
pub type MemoryScanFn = Arc<dyn Fn() -> Boxed<String> + Send + Sync>;
pub type SynthFn = Arc<dyn Fn(State) -> Boxed<String> + Send + Sync>;
pub type DraftFn = Arc<dyn Fn(State) -> Boxed<String> + Send + Sync>;

/// The router: decides on the **counts** scans write back — never on digest prose.
///
/// The engine's rule is "the router is code, not a model". Routing on the model's
/// wording hands control flow to the model, and every branch then needs a scripted
/// reply to be testable. Counts are exact, cheap, and directly drivable in tests.
pub fn needs_action(state: &State) -> String {
    let pending = state.i64("gh_open_prs").unwrap_or(0) > 0
        || state.i64("gh_open_issues").unwrap_or(0) > 0
        || state.i64("cal_event_count").unwrap_or(0) > 0;
    if pending {
        "propose".to_string()
    } else {
        "quiet".to_string()
    }
}

/// The honest copy when a scan fails — "unavailable" is itself information, better than silence.
fn unavailable(why: &str) -> String {
    format!("unavailable ({why})")
}

/// The generic shape of a scan node: run an async scan; Ok → the success keys,
/// Err → the failure keys (honest copy / zeros). The rule **every scan contains
/// its own failure** is written once, in this template.
fn scan_fn<V: 'static>(
    scan: Arc<dyn Fn() -> Boxed<V> + Send + Sync>,
    on_ok: impl Fn(V) -> NodeWrites + Clone + Send + Sync + 'static,
    on_err: impl Fn(&str) -> NodeWrites + Clone + Send + Sync + 'static,
) -> crate::NodeFn {
    Arc::new(move |_ctx: NodeCtx| {
        let scan = scan.clone();
        let (on_ok, on_err) = (on_ok.clone(), on_err.clone());
        Box::pin(async move {
            match scan().await {
                Ok(value) => Ok(on_ok(value)),
                Err(why) => Ok(on_err(&why)),
            }
        })
    })
}

/// A single-text scan: the result (or the unavailable copy) goes into one key.
fn text_scan_fn(
    key: &'static str,
    scan: Arc<dyn Fn() -> Boxed<String> + Send + Sync>,
) -> crate::NodeFn {
    scan_fn(
        scan,
        move |text| writes([(key, json!(text))]),
        move |why| writes([(key, json!(unavailable(why)))]),
    )
}

pub fn build_gather_graph(
    github_fn: GithubScanFn,
    web_fn: WebScanFn,
    calendar_fn: CalendarScanFn,
    memory_fn: MemoryScanFn,
    synth_fn: SynthFn,
    draft_fn: DraftFn,
) -> Result<Graph, GraphError> {
    let mut g = Graph::new("gather");

    g.add_node(Node::new(
        "scan_github",
        "tool",
        scan_fn(
            github_fn,
            |(text, prs, issues)| {
                writes([
                    ("gh_text", json!(text)),
                    ("gh_open_prs", json!(prs)),
                    ("gh_open_issues", json!(issues)),
                ])
            },
            |why| {
                writes([
                    ("gh_text", json!(unavailable(why))),
                    ("gh_open_prs", json!(0)),
                    ("gh_open_issues", json!(0)),
                ])
            },
        ),
    ))?;

    g.add_node(Node::new(
        "scan_web",
        "tool",
        text_scan_fn("web_text", web_fn),
    ))?;

    g.add_node(Node::new(
        "scan_calendar",
        "tool",
        scan_fn(
            calendar_fn,
            |(text, count)| writes([("cal_text", json!(text)), ("cal_event_count", json!(count))]),
            |why| {
                writes([
                    ("cal_text", json!(unavailable(why))),
                    ("cal_event_count", json!(0)),
                ])
            },
        ),
    ))?;

    g.add_node(Node::new(
        "scan_memory",
        "tool",
        text_scan_fn("mem_text", memory_fn),
    ))?;

    // The only model call, **no tools**. See iron rule 1 in the module comment.
    let synth = synth_fn.clone();
    g.add_node(Node::new("synthesize", "llm", {
        Arc::new(move |ctx: NodeCtx| {
            let synth = synth.clone();
            let state = ctx.state;
            Box::pin(async move {
                let digest = synth(state).await?;
                Ok(writes([("digest", json!(digest))]))
            })
        })
    }))?;

    // The graph's only write, and it's a file opened by a human.
    let draft = draft_fn.clone();
    g.add_node(Node::new("draft_digest", "tool", {
        Arc::new(move |ctx: NodeCtx| {
            let draft = draft.clone();
            let state = ctx.state;
            Box::pin(async move {
                let path = draft(state).await?;
                Ok(writes([("draft_path", json!(path))]))
            })
        })
    }))?;

    // The four scans are mutually independent → one parallel wave; synthesize waits for all four.
    for scan in ["scan_github", "scan_web", "scan_calendar", "scan_memory"] {
        g.entry(&[scan])?;
        g.add_edge(scan, "synthesize")?;
    }

    // The router hangs on synthesize: it is the meeting wall; triage's separate
    // gather node would be dead weight here.
    g.add_router(
        "synthesize",
        Arc::new(needs_action),
        &[("propose", "draft_digest"), ("quiet", END)],
    )?;
    g.add_edge("draft_digest", END)?;

    Ok(g)
}

/// Topology as data, for the dashboard — builds the graph with stubs, never runs it.
pub fn gather_topology() -> Topology {
    let noop_github: GithubScanFn = Arc::new(|| Box::pin(async { Ok((String::new(), 0, 0)) }));
    let noop_web: WebScanFn = Arc::new(|| Box::pin(async { Ok(String::new()) }));
    let noop_calendar: CalendarScanFn = Arc::new(|| Box::pin(async { Ok((String::new(), 0)) }));
    let noop_memory: MemoryScanFn = Arc::new(|| Box::pin(async { Ok(String::new()) }));
    let noop_synth: SynthFn = Arc::new(|_s| Box::pin(async { Ok(String::new()) }));
    let noop_draft: DraftFn = Arc::new(|_s| Box::pin(async { Ok(String::new()) }));
    match build_gather_graph(
        noop_github,
        noop_web,
        noop_calendar,
        noop_memory,
        noop_synth,
        noop_draft,
    ) {
        Ok(graph) => graph.describe(),
        // Reaching here means the graph itself is wrong — and it is hard-coded in this file.
        Err(error) => panic!("gather 图写错了：{error}"),
    }
}

/// Assembles one synthesize request with the real provider — the CLI's binding
/// uses it, keeping the final checkpoint of the "no tools" iron rule.
pub fn synth_request(model: &str, state: &State) -> CreateRequest {
    let fill = |key: &str| state.str(key).unwrap_or_default().to_string();
    let prompt = DIGEST_PROMPT
        .replace("{gh_text}", &fill("gh_text"))
        .replace("{web_text}", &fill("web_text"))
        .replace("{cal_text}", &fill("cal_text"))
        .replace("{mem_text}", &fill("mem_text"));
    CreateRequest {
        model: model.to_string(),
        system: None,
        messages: vec![Message::user_text(prompt)],
        tools: Vec::new(),
        max_tokens: 1500,
    }
}

/// The checklist tests use to assert "parallel nodes write disjoint keys" —
/// each node name and the key prefixes it may write.
pub fn scan_owns(scan: &str) -> &'static [&'static str] {
    match scan {
        "scan_github" => &["gh_text", "gh_open_prs", "gh_open_issues"],
        "scan_web" => &["web_text"],
        "scan_calendar" => &["cal_text", "cal_event_count"],
        "scan_memory" => &["mem_text"],
        _ => &[],
    }
}

#[cfg(test)]
#[path = "gather_tests.rs"]
mod gather_tests;
