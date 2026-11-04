//! Common node shapes.
//!
//! Everything was already `NodeFn` (state in, keys-to-merge out) and `Node::new`
//! wraps any closure — dedicated "wrappers" only made the call one line longer.
//!
//! What's truly worth keeping is what `agent_node` says: a node can be **an entire
//! loop**. The graph doesn't replace the loop — it arranges where the loop is
//! called and what else happens around it. triage's `full_agent` is wired exactly
//! that way (see `workflows::triage`), and it calls the very same `full_turn` as
//! without a graph, so "loop as node" and "loop as default" can never drift apart.
//!
//! Likewise not kept: `llm_node` — it stitched a prompt template and state together
//! with `format`, while both places needing it (triage's classify, quick_reply) had
//! to parse the return value or pick a model themselves; handwriting was shorter.

use std::sync::Arc;

use crate::{NodeCtx, NodeFn, NodeWrites, RouteFn, State};

/// First-party router: reads a state key and returns it as the route label (missing
/// or empty → `default`). The model writes the key; this code reads it. Lives here
/// (not in a workflow file) so the declarative route whitelist can reference it.
pub fn key_router(key: impl Into<String>, default: impl Into<String>) -> RouteFn {
    let (key, default) = (key.into(), default.into());
    Arc::new(move |state: &State| match state.str(&key) {
        Some(value) if !value.is_empty() => value.to_string(),
        _ => default.clone(),
    })
}

/// gather's route: decide on the **counts** scans write back — never on digest prose.
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

/// The escape hatch: any **pure** `state in → keys out` function can be a node
/// directly, no hand-written `Box::pin`. Use it for sync work — file reads, DB
/// queries, arithmetic. Nodes that need await (model/tool calls) still box
/// themselves — there boxing is necessary, not ceremony.
pub fn fn_node<F>(f: F) -> NodeFn
where
    F: Fn(&State) -> NodeWrites + Send + Sync + 'static,
{
    Arc::new(move |ctx: NodeCtx| {
        let out = f(&ctx.state);
        Box::pin(async move { Ok(out) })
    })
}
