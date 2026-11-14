//! Declarative-definition loader tests.
//!
//! The loader is the place where a bad workflow must refuse to exist: a static-edge
//! cycle, an unregistered router, a node without a bound implementation — each is a
//! build error, never a runtime surprise. And the happy path is the real thing:
//! `triage.json` built with scripted bindings and **run** end to end.

use std::collections::BTreeMap;

use serde_json::json;

use super::nodes::fn_node;
use super::workflow_def::{builtin_routes, WorkflowDef};
use super::{run_graph, writes, NodeFn, State, DEFAULT_MAX_STEPS};

/// The full binding set triage.json needs, with stubbed behaviour.
fn triage_def() -> WorkflowDef {
    WorkflowDef::parse(include_str!("workflows/definitions/triage.json"))
        .expect("triage.json is legal")
}

fn triage_bindings(route: &'static str) -> BTreeMap<String, NodeFn> {
    let mut bindings: BTreeMap<String, NodeFn> = BTreeMap::new();
    bindings.insert(
        "classify".to_string(),
        fn_node(move |_| writes([("route", json!(route))])),
    );
    bindings.insert(
        "check_calendar".to_string(),
        fn_node(|_| writes([("calendar", json!("(nothing today)"))])),
    );
    bindings.insert("gather".to_string(), fn_node(|_| serde_json::Map::new()));
    bindings.insert(
        "quick_reply".to_string(),
        fn_node(|_| writes([("reply", json!("hi"))])),
    );
    bindings.insert(
        "full_agent".to_string(),
        fn_node(|_| writes([("reply", json!("full path"))])),
    );
    bindings
}

#[tokio::test]
async fn triage_json_builds_and_runs_the_quick_path() {
    let def = triage_def();
    let bindings = triage_bindings("quick");
    let graph = def
        .build(&bindings, &builtin_routes())
        .expect("build the graph");

    let mut state = State::new();
    state.set("message", json!("thanks!"));
    let report = run_graph(graph, state, None, DEFAULT_MAX_STEPS)
        .await
        .expect("run the graph");

    assert_eq!(
        report.path,
        vec!["classify", "check_calendar", "gather", "quick_reply"],
        "the quick branch's route jumps after meeting at gather: {:?}",
        report.path
    );
    assert_eq!(report.state.str("reply"), Some("hi"));
}

#[tokio::test]
async fn triage_json_routes_the_full_path_too() {
    let def = triage_def();
    let bindings = triage_bindings("full");
    let graph = def
        .build(&bindings, &builtin_routes())
        .expect("build the graph");

    let mut state = State::new();
    state.set("message", json!("plan a swim on saturday"));
    let report = run_graph(graph, state, None, DEFAULT_MAX_STEPS)
        .await
        .expect("run the graph");

    assert!(report.path.contains(&"full_agent".to_string()));
    assert!(!report.path.contains(&"quick_reply".to_string()));
    assert_eq!(report.state.str("reply"), Some("full path"));
}

#[test]
fn static_cycle_refuses_to_build() {
    let def = WorkflowDef::parse(
        r#"{
            "name": "bad",
            "nodes": [{ "name": "a" }, { "name": "b" }],
            "entry": ["a"],
            "edges": [["a", "b"], ["b", "a"]]
        }"#,
    )
    .expect("legal JSON");
    let bindings: BTreeMap<String, NodeFn> = BTreeMap::new();
    let error = def
        .build(&bindings, &builtin_routes())
        .err()
        .expect("building the graph should fail");
    assert!(
        error.contains("cycle"),
        "a cycle in the static edges should be reported: {error}"
    );
}

#[test]
fn unregistered_router_refuses_to_build() {
    let def = WorkflowDef::parse(
        r#"{
            "name": "bad",
            "nodes": [{ "name": "a" }],
            "entry": ["a"],
            "routers": [{ "src": "a", "router": "ask_the_model", "targets": { "x": "a" } }]
        }"#,
    )
    .expect("legal JSON");
    let mut bindings: BTreeMap<String, NodeFn> = BTreeMap::new();
    bindings.insert("a".to_string(), fn_node(|_| serde_json::Map::new()));
    let error = def
        .build(&bindings, &builtin_routes())
        .err()
        .expect("building the graph should fail");
    // Control flow never goes to the model: a route name outside the whitelist is refused at load
    // time.
    assert!(
        error.contains("not registered"),
        "an unregistered route should be reported: {error}"
    );
    assert!(error.contains("ask_the_model"));
}

#[test]
fn node_without_binding_refuses_to_build() {
    let def =
        WorkflowDef::parse(r#"{ "name": "bad", "nodes": [{ "name": "a" }], "entry": ["a"] }"#)
            .expect("legal JSON");
    let bindings: BTreeMap<String, NodeFn> = BTreeMap::new();
    let error = def
        .build(&bindings, &builtin_routes())
        .err()
        .expect("building the graph should fail");
    assert!(
        error.contains("no bound implementation"),
        "a missing binding should be reported: {error}"
    );
}

#[test]
fn route_builder_with_bad_params_refuses_to_build() {
    let def = WorkflowDef::parse(
        r#"{
            "name": "bad",
            "nodes": [{ "name": "a" }],
            "entry": ["a"],
            "routers": [{ "src": "a", "router": "key_router", "params": {}, "targets": { "x": "a" } }]
        }"#,
    )
    .expect("legal JSON");
    let mut bindings: BTreeMap<String, NodeFn> = BTreeMap::new();
    bindings.insert("a".to_string(), fn_node(|_| serde_json::Map::new()));
    let error = def
        .build(&bindings, &builtin_routes())
        .err()
        .expect("building the graph should fail");
    assert!(
        error.contains("params.key"),
        "a missing argument should be reported: {error}"
    );
}

#[test]
fn the_route_whitelist_is_exactly_the_two_builtins() {
    let routes = builtin_routes();
    let names: Vec<&String> = routes.keys().collect();
    assert_eq!(names.len(), 2, "the allowlist is closed: {names:?}");
    assert!(names.contains(&&"key_router".to_string()));
    assert!(names.contains(&&"needs_action".to_string()));
}
