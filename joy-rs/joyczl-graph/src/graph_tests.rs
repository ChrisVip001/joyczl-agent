//! The engine's mechanics — pure-function nodes, zero model, zero network.
//!
//! These pin the contracts everything downstream depends on: execution order
//! (`path`), true concurrency within a wave, the disjoint-write-keys rule, both
//! loop guardrails, error draining, and the observer events the dashboard renders.

use std::sync::Arc;
use std::time::Instant;

use serde_json::json;

use super::nodes::{fn_node, key_router};
use super::*;

/// Collects events with Arc<Mutex<Vec<_>>> — the observer is Sync, so are the tests.
type Events = Arc<std::sync::Mutex<Vec<GraphEvent>>>;

fn collect_events() -> (Events, Observer) {
    let events: Events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = events.clone();
    (
        events,
        Arc::new(move |event| sink.lock().expect("lock").push(event)),
    )
}

fn kinds(events: &Events) -> Vec<&'static str> {
    events
        .lock()
        .expect("lock")
        .iter()
        .map(|e| match e {
            GraphEvent::Started { .. } => "graph_start",
            GraphEvent::NodeStarted { .. } => "node_start",
            GraphEvent::NodeEnded { .. } => "node_end",
            GraphEvent::Route { .. } => "route",
            GraphEvent::Inner { .. } => "inner",
            GraphEvent::Ended { .. } => "graph_end",
        })
        .collect()
}

fn ended(events: &Events) -> (i64, i32, Vec<String>, Option<String>) {
    for event in events.lock().expect("lock").iter() {
        if let GraphEvent::Ended {
            ms,
            steps,
            path,
            error,
            ..
        } = event
        {
            return (*ms, *steps, path.clone(), error.clone());
        }
    }
    panic!("there is no graph_end event");
}

/// A pure compute node.
fn pure(name: &str, f: impl Fn(&State) -> NodeWrites + Send + Sync + 'static) -> Node {
    Node::new(name, "fn", fn_node(f))
}

fn linear_graph() -> Graph {
    let mut g = Graph::new("linear");
    g.add_node(pure("a", |_| writes([("a", json!(1))])))
        .unwrap();
    g.add_node(pure("b", |s| {
        writes([("b", json!(s.i64("a").unwrap() + 1))])
    }))
    .unwrap();
    g.add_node(pure("c", |s| {
        writes([("c", json!(s.i64("b").unwrap() + 1))])
    }))
    .unwrap();
    g.entry(&["a"]).unwrap();
    g.add_edge("a", "b").unwrap();
    g.add_edge("b", "c").unwrap();
    g.add_edge("c", END).unwrap();
    g
}

fn routed_graph() -> Graph {
    let mut g = Graph::new("routed");
    g.add_node(pure("decide", |s| {
        writes([("route", s.get("want").cloned().unwrap())])
    }))
    .unwrap();
    g.add_node(pure("quick", |_| writes([("took", json!("quick"))])))
        .unwrap();
    g.add_node(pure("full", |_| writes([("took", json!("full"))])))
        .unwrap();
    g.entry(&["decide"]).unwrap();
    g.add_router(
        "decide",
        key_router("route", "full"),
        &[("quick", "quick"), ("full", "full")],
    )
    .unwrap();
    g.add_edge("quick", END).unwrap();
    g.add_edge("full", END).unwrap();
    g
}

fn cycle_graph(max_visits: i32) -> Graph {
    let mut g = Graph::new("cycle");
    g.add_node(
        pure("work", |s| {
            writes([("count", json!(s.i64("count").unwrap_or(0) + 1))])
        })
        .max_visits(max_visits),
    )
    .unwrap();
    g.entry(&["work"]).unwrap();
    g.add_router(
        "work",
        Arc::new(|s: &State| {
            if s.i64("count").unwrap_or(0) < 99 {
                "again".to_string()
            } else {
                "done".to_string()
            }
        }),
        &[("again", "work"), ("done", END)],
    )
    .unwrap();
    g
}

#[tokio::test]
async fn linear_topology_runs_in_edge_order() {
    let (events, observer) = collect_events();
    let report = run_graph(
        linear_graph(),
        State::new(),
        Some(observer),
        DEFAULT_MAX_STEPS,
    )
    .await
    .expect("it runs");
    assert_eq!(
        (
            report.state.i64("a"),
            report.state.i64("b"),
            report.state.i64("c")
        ),
        (Some(1), Some(2), Some(3))
    );
    let (_, _, path, error) = ended(&events);
    assert_eq!(path, vec!["a", "b", "c"]);
    assert_eq!(error, None);
}

#[tokio::test]
async fn fanout_runs_concurrently_and_fanin_waits_for_all() {
    let mut g = Graph::new("fan");
    for key in ["x", "y", "z"] {
        g.add_node(Node::new(
            key,
            "fn",
            Arc::new(move |_ctx: NodeCtx| {
                Box::pin(async move {
                    // Each branch writes its own key, after waiting 150ms.
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    Ok(writes([(key, json!(key))]))
                })
            }),
        ))
        .unwrap();
        g.entry(&[key]).unwrap();
    }
    // fan-in waits for all three to fire before the three keys become visible.
    g.add_node(pure("join", |s| {
        writes([(
            "joined",
            json!(format!(
                "{}{}{}",
                s.str("x").unwrap_or("?"),
                s.str("y").unwrap_or("?"),
                s.str("z").unwrap_or("?")
            )),
        )])
    }))
    .unwrap();
    for key in ["x", "y", "z"] {
        g.add_edge(key, "join").unwrap();
    }

    let started = Instant::now();
    let report = run_graph(g, State::new(), None, DEFAULT_MAX_STEPS)
        .await
        .expect("it runs");
    let elapsed = started.elapsed();
    assert_eq!(report.state.str("joined"), Some("xyz")); // fan-in saw all three keys
    assert!(
        elapsed.as_millis() < 350,
        "three 150 ms nodes should run side by side, it took {elapsed:?}"
    );
}

#[tokio::test]
async fn router_picks_the_target_from_state() {
    let (events, observer) = collect_events();
    let report = run_graph(
        routed_graph(),
        State::from_value(json!({"want": "quick"})).unwrap(),
        Some(observer),
        DEFAULT_MAX_STEPS,
    )
    .await
    .expect("it runs");
    assert_eq!(report.state.str("took"), Some("quick"));
    let route = events
        .lock()
        .expect("lock")
        .iter()
        .find_map(|e| match e {
            GraphEvent::Route { target, reason, .. } => Some((target.clone(), reason.clone())),
            _ => None,
        })
        .expect("there is a route event");
    assert_eq!(route, ("quick".to_string(), "quick".to_string()));

    let full = run_graph(
        routed_graph(),
        State::from_value(json!({"want": "full"})).unwrap(),
        None,
        DEFAULT_MAX_STEPS,
    )
    .await
    .expect("it runs");
    assert_eq!(full.state.str("took"), Some("full"));
}

#[tokio::test]
async fn router_unknown_label_records_error_and_drains() {
    let (events, observer) = collect_events();
    let report = run_graph(
        routed_graph(),
        State::from_value(json!({"want": "sideways"})).unwrap(),
        Some(observer),
        DEFAULT_MAX_STEPS,
    )
    .await
    .expect("it runs");
    assert!(!report.state.has("took"));
    assert!(report.errors["decide"].contains("unknown label"));
    assert!(ended(&events).3.is_some());
}

#[test]
fn unknown_edge_and_router_targets_error_at_build_time() {
    let mut g = Graph::new("bad");
    g.add_node(pure("a", |_| NodeWrites::new())).unwrap();
    assert!(matches!(
        g.add_edge("a", "ghost"),
        Err(GraphError::UnknownNode { .. })
    ));
    assert!(matches!(
        g.add_router("a", key_router("k", "x"), &[("x", "ghost")]),
        Err(GraphError::UnknownRouterTarget { .. })
    ));
    assert!(matches!(
        g.add_node(pure(START, |_| NodeWrites::new())),
        Err(GraphError::Reserved { .. })
    ));
}

#[tokio::test]
async fn parallel_key_collision_is_fatal() {
    let mut g = Graph::new("collide");
    g.add_node(pure("left", |_| writes([("same", json!(1))])))
        .unwrap();
    g.add_node(pure("right", |_| writes([("same", json!(2))])))
        .unwrap();
    g.entry(&["left", "right"]).unwrap();
    // Two same-wave nodes writing one key is a graph bug; dropping a write silently is worse.
    let error = run_graph(g, State::new(), None, DEFAULT_MAX_STEPS)
        .await
        .expect_err("two nodes colliding on a key must error");
    assert!(matches!(error, GraphError::Collision { key, .. } if key == "same"));
}

#[tokio::test]
async fn sequential_overwrite_is_allowed() {
    // In sequential execution a later node overwriting an earlier key is fine (only parallel is banned).
    let report = run_graph(
        linear_graph(),
        State::from_value(json!({"a": 99})).unwrap(),
        None,
        DEFAULT_MAX_STEPS,
    )
    .await
    .expect("it runs");
    assert_eq!(report.state.i64("a"), Some(1));
}

#[tokio::test]
async fn cycle_stops_at_max_visits() {
    let report = run_graph(cycle_graph(3), State::new(), None, DEFAULT_MAX_STEPS)
        .await
        .expect("it runs");
    assert_eq!(report.state.i64("count"), Some(3));
    assert!(report.errors["work"].contains("max_visits=3"));
}

#[tokio::test]
async fn max_steps_is_the_global_hard_stop() {
    let (events, observer) = collect_events();
    let report = run_graph(cycle_graph(99), State::new(), Some(observer), 5)
        .await
        .expect("it runs");
    assert_eq!(report.state.i64("count"), Some(5));
    assert!(report.errors["engine"].contains("max_steps=5"));
    assert_eq!(ended(&events).1, 5);
}

#[tokio::test]
async fn node_error_routes_to_on_error_and_never_escapes() {
    let mut g = Graph::new("boom");
    g.add_node(
        Node::new(
            "explode",
            "fn",
            Arc::new(|_ctx: NodeCtx| {
                Box::pin(async { Err("ZeroDivisionError: division by zero".to_string()) })
            }),
        )
        .on_error("fallback"),
    )
    .unwrap();
    g.add_node(pure("fallback", |_| writes([("saved", json!(true))])))
        .unwrap();
    g.entry(&["explode"]).unwrap();
    g.add_edge("fallback", END).unwrap();

    let report = run_graph(g, State::new(), None, DEFAULT_MAX_STEPS)
        .await
        .expect("a failing node must not blow up the run");
    assert_eq!(report.state.get("saved"), Some(&json!(true)));
    assert!(report.errors["explode"].contains("ZeroDivisionError"));
    assert_eq!(report.path, vec!["explode", "fallback"]);
}

#[tokio::test]
async fn node_error_without_on_error_drains_to_end() {
    let mut g = Graph::new("boom2");
    g.add_node(Node::new(
        "explode",
        "fn",
        Arc::new(|_ctx: NodeCtx| Box::pin(async { Err("boom".to_string()) })),
    ))
    .unwrap();
    g.add_node(pure("after", |_| writes([("ran", json!(true))])))
        .unwrap();
    g.entry(&["explode"]).unwrap();
    g.add_edge("explode", "after").unwrap();

    let (events, observer) = collect_events();
    let report = run_graph(g, State::new(), Some(observer), DEFAULT_MAX_STEPS)
        .await
        .expect("it runs");
    assert!(!report.state.has("ran")); // downstream starved, the run ends cleanly
    assert!(ended(&events).3.is_some());
}

#[tokio::test]
async fn observer_event_sequence_is_pinned() {
    // Pins the event kinds and order — the dashboard's graph view renders from this.
    let (events, observer) = collect_events();
    run_graph(
        routed_graph(),
        State::from_value(json!({"want": "quick"})).unwrap(),
        Some(observer),
        DEFAULT_MAX_STEPS,
    )
    .await
    .expect("it runs");
    assert_eq!(
        kinds(&events),
        vec![
            "graph_start",
            "node_start",
            "node_end",
            "route",
            "node_start",
            "node_end",
            "graph_end"
        ]
    );

    let first = events.lock().expect("lock")[0].clone();
    match first {
        GraphEvent::Started { workflow, nodes } => {
            assert_eq!(workflow, "routed");
            assert_eq!(nodes, vec!["decide", "quick", "full"]);
        }
        other => panic!("the first event should be graph_start, got {other:?}"),
    }
}

#[test]
fn describe_reports_every_node_and_edge() {
    let topology = routed_graph().describe();
    assert_eq!(topology.name, "routed");
    let names: Vec<&str> = topology.nodes.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(names, vec!["decide", "quick", "full"]);
    let mut conditional: Vec<(&str, &str)> = topology
        .edges
        .iter()
        .filter(|e| e.conditional)
        .map(|e| (e.src.as_str(), e.dst.as_str()))
        .collect();
    // The order of a router's target edges isn't part of the contract (the
    // implementation sorts by key), so only "these are the edges" is pinned.
    conditional.sort_unstable();
    let statik: Vec<(&str, &str)> = topology
        .edges
        .iter()
        .filter(|e| !e.conditional)
        .map(|e| (e.src.as_str(), e.dst.as_str()))
        .collect();
    assert_eq!(conditional, vec![("decide", "full"), ("decide", "quick")]);
    assert_eq!(
        statik,
        vec![(START, "decide"), ("quick", END), ("full", END)]
    );
}

#[tokio::test]
async fn reserved_underscore_keys_never_merge_or_leak() {
    let mut g = Graph::new("private");
    g.add_node(pure("sneaky", |_| {
        writes([("_secret", json!(1)), ("open", json!(2))])
    }))
    .unwrap();
    g.entry(&["sneaky"]).unwrap();

    let (events, observer) = collect_events();
    let report = run_graph(g, State::new(), Some(observer), DEFAULT_MAX_STEPS)
        .await
        .expect("it runs");
    assert!(!report.state.has("_secret"));
    assert_eq!(report.state.i64("open"), Some(2));
    let keys = events
        .lock()
        .expect("lock")
        .iter()
        .find_map(|e| match e {
            GraphEvent::NodeEnded { keys, .. } => Some(keys.clone()),
            _ => None,
        })
        .expect("there is a node_end");
    assert_eq!(keys, vec!["open"]);
}

#[tokio::test]
async fn inner_events_come_out_tagged_with_their_node() {
    // Internal node events (loop's llm / tool / text) must be traceable to their node.
    let mut g = Graph::new("tagged");
    g.add_node(Node::new(
        "agent",
        "agent",
        Arc::new(|ctx: NodeCtx| {
            let inner = ctx.inner.clone();
            Box::pin(async move {
                inner(joyczl_loop::LoopEvent::Text {
                    delta: "hi".to_string(),
                });
                Ok(writes([("out", json!("ok"))]))
            })
        }),
    ))
    .unwrap();
    g.entry(&["agent"]).unwrap();

    let (events, observer) = collect_events();
    run_graph(g, State::new(), Some(observer), DEFAULT_MAX_STEPS)
        .await
        .expect("it runs");
    let inner = events
        .lock()
        .expect("lock")
        .iter()
        .find_map(|e| match e {
            GraphEvent::Inner { node, event } => Some((node.clone(), event.clone())),
            _ => None,
        })
        .expect("there is an inner event");
    assert_eq!(inner.0, "agent");
    assert_eq!(
        inner.1,
        joyczl_loop::LoopEvent::Text {
            delta: "hi".to_string()
        }
    );
}

#[test]
fn state_round_trips_through_json() {
    let state = State::from_value(json!({"a": 1, "b": "x"})).unwrap();
    assert_eq!(state.i64("a"), Some(1));
    assert_eq!(state.str("b"), Some("x"));
    assert_eq!(state.into_value(), json!({"a": 1, "b": "x"}));
    assert!(State::from_value(json!([1, 2, 3])).is_err());
}

#[tokio::test]
async fn errors_from_a_previous_run_are_kept() {
    // State is reusable: errors left by the previous run must not be silently cleared.
    let state = State::from_value(json!({"errors": {"old": "earlier"}})).unwrap();
    let report = run_graph(linear_graph(), state, None, DEFAULT_MAX_STEPS)
        .await
        .expect("it runs");
    assert_eq!(report.errors["old"], "earlier");
    assert_eq!(report.state.get("errors"), Some(&json!({"old": "earlier"})));
}
