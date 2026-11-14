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

use std::collections::BTreeMap;
use std::sync::Arc;

use joyczl_provider::{CreateRequest, Message};
use serde_json::json;

use crate::workflow_def::{builtin_routes, WorkflowDef};
use crate::{writes, Boxed, Graph, NodeCtx, NodeFn, NodeWrites, State, Topology};

/// The one definition of the topology, embedded at compile time (`definitions/gather.json`).
/// A workflow is part of the code and ships with the binary — changes go through review,
/// not through runtime loading.
const GATHER_DEF: &str = include_str!("definitions/gather.json");

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

/// The router lives in `nodes.rs` (so the declarative route whitelist can reference
/// it); re-exported here because gather is its only user today.
pub use crate::nodes::needs_action;

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
) -> Result<Graph, String> {
    // The topology lives in definitions/gather.json (embedded at compile time, validated at
    // startup); this file only attaches behaviour by node name — JSON declares the shape, code
    // provides the action.
    let mut bindings: BTreeMap<String, NodeFn> = BTreeMap::new();

    bindings.insert(
        "scan_github".to_string(),
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
    );

    bindings.insert("scan_web".to_string(), text_scan_fn("web_text", web_fn));

    bindings.insert(
        "scan_calendar".to_string(),
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
    );

    bindings.insert(
        "scan_memory".to_string(),
        text_scan_fn("mem_text", memory_fn),
    );

    // The only model call, **no tools**. See iron rule 1 in the module comment.
    bindings.insert(
        "synthesize".to_string(),
        Arc::new(move |ctx: NodeCtx| {
            let synth = synth_fn.clone();
            let state = ctx.state;
            Box::pin(async move {
                let digest = synth(state).await?;
                Ok(writes([("digest", json!(digest))]))
            })
        }),
    );

    // The graph's only write, and it's a file opened by a human.
    bindings.insert(
        "draft_digest".to_string(),
        Arc::new(move |ctx: NodeCtx| {
            let draft = draft_fn.clone();
            let state = ctx.state;
            Box::pin(async move {
                let path = draft(state).await?;
                Ok(writes([("draft_path", json!(path))]))
            })
        }),
    );

    WorkflowDef::parse(GATHER_DEF)?.build(&bindings, &builtin_routes())
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
        Err(error) => panic!("the gather graph is wrong: {error}"),
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
        cache_prefix: None,
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
