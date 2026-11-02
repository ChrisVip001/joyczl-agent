//! 引擎的机械部分 —— 纯函数节点，零模型、零网络。
//!
//! 这些钉住的是下游全都依赖的契约：执行顺序（`path`）、一波之内的真并发、
//! 写键不相交的规矩、两道循环护栏、出错排空、以及 dashboard 要渲染的
//! 观察者事件序列。

use std::sync::Arc;
use std::time::Instant;

use serde_json::json;

use super::nodes::{fn_node, key_router};
use super::*;

/// 用 Arc<Mutex<Vec<_>>> 收集事件 —— 观察者是 Sync 的，测试里也是这样。
type Events = Arc<std::sync::Mutex<Vec<GraphEvent>>>;

fn collect_events() -> (Events, Observer) {
    let events: Events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = events.clone();
    (
        events,
        Arc::new(move |event| sink.lock().expect("锁").push(event)),
    )
}

fn kinds(events: &Events) -> Vec<&'static str> {
    events
        .lock()
        .expect("锁")
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
    for event in events.lock().expect("锁").iter() {
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
    panic!("没有 graph_end 事件");
}

/// 纯计算节点。
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
    .expect("能跑");
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
                    // 每个分支写自己的键，先等 150ms。
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    Ok(writes([(key, json!(key))]))
                })
            }),
        ))
        .unwrap();
        g.entry(&[key]).unwrap();
    }
    // fan-in 等三个都点着，然后才看得到三个键。
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
        .expect("能跑");
    let elapsed = started.elapsed();
    assert_eq!(report.state.str("joined"), Some("xyz")); // fan-in 看到了三个键
    assert!(
        elapsed.as_millis() < 350,
        "三个 150ms 的节点应当是并排跑的，实际用了 {elapsed:?}"
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
    .expect("能跑");
    assert_eq!(report.state.str("took"), Some("quick"));
    let route = events
        .lock()
        .expect("锁")
        .iter()
        .find_map(|e| match e {
            GraphEvent::Route { target, reason, .. } => Some((target.clone(), reason.clone())),
            _ => None,
        })
        .expect("有 route 事件");
    assert_eq!(route, ("quick".to_string(), "quick".to_string()));

    let full = run_graph(
        routed_graph(),
        State::from_value(json!({"want": "full"})).unwrap(),
        None,
        DEFAULT_MAX_STEPS,
    )
    .await
    .expect("能跑");
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
    .expect("能跑");
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
    // 同一波里两个节点写同一个键 —— 这是图的 bug，静默丢一次写比报错糟得多。
    let error = run_graph(g, State::new(), None, DEFAULT_MAX_STEPS)
        .await
        .expect_err("两个节点撞键必须报错");
    assert!(matches!(error, GraphError::Collision { key, .. } if key == "same"));
}

#[tokio::test]
async fn sequential_overwrite_is_allowed() {
    // 顺序执行时后一个节点覆盖前一个的键完全正常（并行才禁止）。
    let report = run_graph(
        linear_graph(),
        State::from_value(json!({"a": 99})).unwrap(),
        None,
        DEFAULT_MAX_STEPS,
    )
    .await
    .expect("能跑");
    assert_eq!(report.state.i64("a"), Some(1));
}

#[tokio::test]
async fn cycle_stops_at_max_visits() {
    let report = run_graph(cycle_graph(3), State::new(), None, DEFAULT_MAX_STEPS)
        .await
        .expect("能跑");
    assert_eq!(report.state.i64("count"), Some(3));
    assert!(report.errors["work"].contains("max_visits=3"));
}

#[tokio::test]
async fn max_steps_is_the_global_hard_stop() {
    let (events, observer) = collect_events();
    let report = run_graph(cycle_graph(99), State::new(), Some(observer), 5)
        .await
        .expect("能跑");
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
        .expect("节点出错不该把 run 掀了");
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
        .expect("能跑");
    assert!(!report.state.has("ran")); // 下游被饿死，运行干净地结束
    assert!(ended(&events).3.is_some());
}

#[tokio::test]
async fn observer_event_sequence_is_pinned() {
    // 钉死事件种类与顺序 —— dashboard 的图视图就照这个渲染。
    let (events, observer) = collect_events();
    run_graph(
        routed_graph(),
        State::from_value(json!({"want": "quick"})).unwrap(),
        Some(observer),
        DEFAULT_MAX_STEPS,
    )
    .await
    .expect("能跑");
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

    let first = events.lock().expect("锁")[0].clone();
    match first {
        GraphEvent::Started { workflow, nodes } => {
            assert_eq!(workflow, "routed");
            assert_eq!(nodes, vec!["decide", "quick", "full"]);
        }
        other => panic!("第一个事件应当是 graph_start，拿到 {other:?}"),
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
    // 同一个路由器的几条目标边谁先谁后不属于契约（实现里是按键名排序的），
    // 所以只钉「是这几条」。
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
        .expect("能跑");
    assert!(!report.state.has("_secret"));
    assert_eq!(report.state.i64("open"), Some(2));
    let keys = events
        .lock()
        .expect("锁")
        .iter()
        .find_map(|e| match e {
            GraphEvent::NodeEnded { keys, .. } => Some(keys.clone()),
            _ => None,
        })
        .expect("有 node_end");
    assert_eq!(keys, vec!["open"]);
}

#[tokio::test]
async fn inner_events_come_out_tagged_with_their_node() {
    // 节点内部的事件（loop 的 llm / tool / text）要能被追到是哪个节点发的。
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
        .expect("能跑");
    let inner = events
        .lock()
        .expect("锁")
        .iter()
        .find_map(|e| match e {
            GraphEvent::Inner { node, event } => Some((node.clone(), event.clone())),
            _ => None,
        })
        .expect("有 inner 事件");
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
    // state 是可以复用的：上一次留下的 errors 不该被悄悄清掉。
    let state = State::from_value(json!({"errors": {"old": "earlier"}})).unwrap();
    let report = run_graph(linear_graph(), state, None, DEFAULT_MAX_STEPS)
        .await
        .expect("能跑");
    assert_eq!(report.errors["old"], "earlier");
    assert_eq!(report.state.get("errors"), Some(&json!({"old": "earlier"})));
}
