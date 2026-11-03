//! THE GRAPH — nodes, edges, waves. This file is the whole mechanism.
//!
//! A graph workflow is a map of steps: each node does one thing, each edge says
//! "when this is done, go here". The engine runs it in waves:
//!
//! ```text
//! while any node is ready:
//!     run every ready node in this wave (concurrently if several)
//!     merge what they wrote into the shared state
//!     fire their edges / ask their routers where to go next
//! ```
//!
//! That's it. Three things carry everything:
//!
//!   state    a plain map (blackboard). Each node reads it, returns the keys it
//!            wants merged; parallel nodes must write **disjoint** keys — a collision
//!            errors out instead of silently dropping a write.
//!   routers  plain functions over state. The model writes state; code reads it
//!            and picks the edge. Control flow is never decided by the model.
//!   guards   loop's double guardrail, generalized: per-node max_visits (bounded
//!            loops) plus a global max_steps (never spin). Node errors are recorded
//!            and surfaced, never thrown from run — same rule as ToolRegistry.execute.
//!
//! Waves trade a little pipeline-ness for a lot of readability: execution order is
//! deterministic, so a trace reads the same twice and tests can pin paths.
//!
//! observer is an enum event, not a `(kind, dict)` — same reason as loop: typed
//! fields can't be misspelled. The observer itself is Sync; the disk side takes locks.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use futures_util::future::join_all;
use joyczl_loop::{LoopEvent, Observer as LoopObserver};
use serde::Serialize;
use serde_json::{Map, Value};

/// The graph's start: a virtual node that just fires the first batch of nodes.
pub const START: &str = "START";
/// The graph's end: reaching it wraps up; it produces no nodes.
pub const END: &str = "END";

/// Default global step cap: hitting it means the graph didn't converge.
pub const DEFAULT_MAX_STEPS: usize = 25;

/// Key-value pairs written back by a node.
pub type NodeWrites = Map<String, Value>;

/// A node function's return: either a batch of writes, or why it couldn't.
pub type Boxed<T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send>>;

/// What a node receives: a state snapshot plus an outlet.
pub struct NodeCtx {
    /// A snapshot of the blackboard. Mutating it affects nobody; keep things by
    /// **returning** them to the engine.
    pub state: State,
    /// The node's internal events (loop's llm / text / tool) exit here; the engine
    /// tags them with node= and forwards them to the graph's observer.
    pub inner: LoopObserver,
}

pub type NodeFn = Arc<dyn Fn(NodeCtx) -> Boxed<NodeWrites> + Send + Sync>;

/// Router function: reads the **current** contents of state, returns an edge label.
/// It is code, not a model.
pub type RouteFn = Arc<dyn Fn(&State) -> String + Send + Sync>;

/// The graph's observer. As with loop, Arc is so it can move into 'static closures.
pub type Observer = Arc<dyn Fn(GraphEvent) + Send + Sync>;

/// Events the graph emits. Same rule as loop: the gateway draws UI with them,
/// the trace persists them; neither needs to be wired into the graph's own logic.
#[derive(Debug, Clone, PartialEq)]
pub enum GraphEvent {
    Started {
        workflow: String,
        nodes: Vec<String>,
    },
    NodeStarted {
        workflow: String,
        node: String,
        /// Which visit this is for the node (>1 means it's in a loop).
        visit: i32,
    },
    NodeEnded {
        workflow: String,
        node: String,
        ms: i64,
        /// Keys written back this time (private underscore-prefixed keys excluded).
        keys: Vec<String>,
        error: Option<String>,
    },
    Route {
        workflow: String,
        router: String,
        /// The node jumped to; END when the router returned an unknown label.
        target: String,
        /// The label the router returned itself.
        reason: String,
    },
    /// Internal events emitted by the node itself (llm / text / tool inside
    /// full_agent); the engine tags node= and forwards them out.
    Inner { node: String, event: LoopEvent },
    Ended {
        workflow: String,
        ms: i64,
        steps: i32,
        path: Vec<String>,
        error: Option<String>,
    },
}

/// The blackboard. Nodes read and write it; routers read it.
///
/// The implementation is just a `BTreeMap<String, Value>`: keys are ordered, so
/// merged parallel writes keep a deterministic order (trace and tests both benefit).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State(Map<String, Value>);

impl State {
    pub fn new() -> Self {
        State(Map::new())
    }

    /// Builds state from a JSON object; a non-object is the caller's bug.
    pub fn from_value(value: Value) -> Result<Self, String> {
        match value {
            Value::Object(map) => Ok(State(map)),
            other => Err(format!("state 必须是 JSON 对象，拿到 {}", kind_of(&other))),
        }
    }

    pub fn into_value(self) -> Value {
        Value::Object(self.0)
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    pub fn has(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    pub fn set(&mut self, key: &str, value: Value) {
        self.0.insert(key.to_string(), value);
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Value::as_str)
    }

    pub fn i64(&self, key: &str) -> Option<i64> {
        self.0.get(key).and_then(Value::as_i64)
    }

    /// Deserializes a key. A wrong type means a node wrote garbage — error out, don't guess.
    pub fn get_as<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
        serde_json::from_value(self.0.get(key)?.clone()).ok()
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// Merges a batch of writes. Underscore-prefixed keys are node/engine private, never leave the graph.
    pub fn merge(&mut self, writes: NodeWrites) {
        for (key, value) in writes {
            if !key.starts_with('_') {
                self.0.insert(key, value);
            }
        }
    }
}

/// Writes a batch of key-value pairs. The exit helper for node functions, no hand-rolled Map.
pub fn writes(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> NodeWrites {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// One node in the graph.
pub struct Node {
    pub name: String,
    /// tool / llm / agent / router — a label for describe().
    pub kind: String,
    /// >1 only on nodes inside **intentional** loops.
    pub max_visits: i32,
    /// Where to jump on error. Unset means drift to END (same rule as loop's guardrail).
    pub on_error: Option<String>,
    f: NodeFn,
}

impl Node {
    pub fn new(name: &str, kind: &str, f: NodeFn) -> Self {
        Node {
            name: name.to_string(),
            kind: kind.to_string(),
            max_visits: 1,
            on_error: None,
            f,
        }
    }

    pub fn max_visits(mut self, visits: i32) -> Self {
        self.max_visits = visits;
        self
    }

    pub fn on_error(mut self, node: &str) -> Self {
        self.on_error = Some(node.to_string());
        self
    }
}

/// An unconditional edge.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Edge {
    pub src: String,
    pub dst: String,
}

/// An edge in the topology (plus a conditional flag) — data for the dashboard to draw.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TopologyEdge {
    pub src: String,
    pub dst: String,
    pub conditional: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TopologyNode {
    pub name: String,
    pub kind: String,
}

/// Topology as data. The UI draws from this data (not from a hand-copied picture),
/// so the graph never drifts from the code.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Topology {
    pub name: String,
    pub nodes: Vec<TopologyNode>,
    pub edges: Vec<TopologyEdge>,
}

struct Router {
    route: RouteFn,
    targets: BTreeMap<String, String>,
}

/// The graph itself: nodes, edges, routers. Read-only once built.
pub struct Graph {
    name: String,
    /// Nodes kept in a Vec + index map: **insertion order** is part of wave semantics.
    nodes: Vec<Node>,
    index: BTreeMap<String, usize>,
    edges: Vec<Edge>,
    routers: BTreeMap<String, Router>,
}

/// Structural graph errors (catchable at build time, or two same-wave nodes colliding on a key).
#[derive(Debug, Clone, PartialEq)]
pub enum GraphError {
    /// START / END are reserved names.
    Reserved {
        name: String,
    },
    UnknownNode {
        name: String,
    },
    UnknownRouterTarget {
        label: String,
        target: String,
    },
    /// Two nodes in the same wave wrote the same key — a graph bug, not a race.
    Collision {
        key: String,
        nodes: Vec<String>,
    },
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphError::Reserved { name } => write!(f, "'{name}' 是保留名"),
            GraphError::UnknownNode { name } => write!(f, "未知节点 '{name}'"),
            GraphError::UnknownRouterTarget { label, target } => {
                write!(f, "路由目标 '{target}'（标签 '{label}'）未知")
            }
            GraphError::Collision { key, nodes } => write!(
                f,
                "'{}' 和 '{}' 都写了 '{key}' —— 并行分支必须写不相交的键",
                nodes.first().map(String::as_str).unwrap_or("?"),
                nodes.get(1).map(String::as_str).unwrap_or("?"),
            ),
        }
    }
}

impl std::error::Error for GraphError {}

impl Graph {
    pub fn new(name: &str) -> Self {
        Graph {
            name: name.to_string(),
            nodes: Vec::new(),
            index: BTreeMap::new(),
            edges: Vec::new(),
            routers: BTreeMap::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn add_node(&mut self, node: Node) -> Result<&mut Self, GraphError> {
        if node.name == START || node.name == END {
            return Err(GraphError::Reserved {
                name: node.name.clone(),
            });
        }
        self.index.insert(node.name.clone(), self.nodes.len());
        self.nodes.push(node);
        Ok(self)
    }

    /// Unconditional edge: when src finishes, dst moves one step closer to ready.
    /// Add nodes before edges — unknown endpoints error here, not mid-run.
    pub fn add_edge(&mut self, src: &str, dst: &str) -> Result<&mut Self, GraphError> {
        for end in [src, dst] {
            if !self.has(end) && end != START && end != END {
                return Err(GraphError::UnknownNode {
                    name: end.to_string(),
                });
            }
        }
        self.edges.push(Edge {
            src: src.to_string(),
            dst: dst.to_string(),
        });
        Ok(self)
    }

    /// Conditional edge: when src finishes, `route(state)` returns a label and
    /// execution jumps to targets[label]. The router is code, never a model.
    pub fn add_router(
        &mut self,
        src: &str,
        route: RouteFn,
        targets: &[(&str, &str)],
    ) -> Result<&mut Self, GraphError> {
        if !self.has(src) {
            return Err(GraphError::UnknownNode {
                name: src.to_string(),
            });
        }
        let mut map = BTreeMap::new();
        for (label, dst) in targets {
            if !self.has(dst) && *dst != END {
                return Err(GraphError::UnknownRouterTarget {
                    label: label.to_string(),
                    target: dst.to_string(),
                });
            }
            map.insert(label.to_string(), dst.to_string());
        }
        self.routers.insert(
            src.to_string(),
            Router {
                route,
                targets: map,
            },
        );
        Ok(self)
    }

    /// Entry: START → these nodes. Equivalent to a batch of `add_edge(START, ..)`,
    /// but it reads as "where the graph begins".
    pub fn entry(&mut self, nodes: &[&str]) -> Result<&mut Self, GraphError> {
        for name in nodes {
            self.add_edge(START, name)?;
        }
        Ok(self)
    }

    fn has(&self, name: &str) -> bool {
        self.index.contains_key(name)
    }

    fn get(&self, name: &str) -> Option<&Node> {
        self.index.get(name).map(|i| &self.nodes[*i])
    }

    /// Node names, in insertion order.
    pub fn node_names(&self) -> Vec<String> {
        self.nodes.iter().map(|n| n.name.clone()).collect()
    }

    /// Each node's static in-edges. END has none (reaching END is wrapping up).
    fn deps(&self) -> BTreeMap<String, BTreeSet<String>> {
        let mut deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for edge in &self.edges {
            if edge.dst != END {
                deps.entry(edge.dst.clone())
                    .or_default()
                    .insert(edge.src.clone());
            }
        }
        deps
    }

    /// Topology as data.
    pub fn describe(&self) -> Topology {
        let mut edges: Vec<TopologyEdge> = self
            .edges
            .iter()
            .map(|e| TopologyEdge {
                src: e.src.clone(),
                dst: e.dst.clone(),
                conditional: false,
            })
            .collect();
        for (src, router) in &self.routers {
            // dict.fromkeys: one edge per distinct target.
            let mut seen: BTreeSet<&str> = BTreeSet::new();
            for dst in router.targets.values() {
                if seen.insert(dst.as_str()) {
                    edges.push(TopologyEdge {
                        src: src.clone(),
                        dst: dst.clone(),
                        conditional: true,
                    });
                }
            }
        }
        Topology {
            name: self.name.clone(),
            nodes: self
                .nodes
                .iter()
                .map(|n| TopologyNode {
                    name: n.name.clone(),
                    kind: n.kind.clone(),
                })
                .collect(),
            edges,
        }
    }
}

/// The harvest of one graph run.
#[derive(Debug, Clone)]
pub struct RunReport {
    /// The state after the run. Keys like `result` / `reply` are the graph's output.
    pub state: State,
    /// The execution path, in actual order — what tests pin.
    pub path: Vec<String>,
    /// Each node's own errors, plus the engine's own (under the `engine` key).
    pub errors: BTreeMap<String, String>,
    pub ms: i64,
}

impl RunReport {
    /// The first error (`errors` is a BTreeMap, ordered by key — unlike Python's
    /// insertion order, but its use is only "did anything fail, and what", not order).
    pub fn first_error(&self) -> Option<&str> {
        self.errors.values().next().map(String::as_str)
    }
}

/// Runs a graph to completion. Returns the final state; errors land in
/// `state["errors"]` and the `graph_end` event, never thrown.
///
/// The one exception is a node key collision ([`GraphError::Collision`]) — that's
/// a graph bug; silently dropping a write is worse than erroring.
pub async fn run_graph(
    graph: Graph,
    state: State,
    observer: Option<Observer>,
    max_steps: usize,
) -> Result<RunReport, GraphError> {
    let raw: Observer = observer.unwrap_or_else(|| Arc::new(|_| {}));
    let t0 = Instant::now();
    let deps = graph.deps();
    let mut fired: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut runs: BTreeMap<String, i32> = BTreeMap::new();
    let mut path: Vec<String> = Vec::new();
    // Errors left by a previous run must be kept (state may be reused).
    let mut errors: BTreeMap<String, String> = existing_errors(&state);

    let mut state = state;
    raw(GraphEvent::Started {
        workflow: graph.name.clone(),
        nodes: graph.node_names(),
    });

    // START fires its own edges first, then the first wave is computed.
    for edge in &graph.edges {
        if edge.src == START {
            fired
                .entry(edge.dst.clone())
                .or_default()
                .insert(START.to_string());
        }
    }

    let mut wave = next_wave(&graph, &deps, &fired, &runs, &[], &mut errors);

    while !wave.is_empty() {
        if path.len() + wave.len() > max_steps {
            errors.insert(
                "engine".to_string(),
                format!("max_steps={max_steps} reached"),
            );
            break;
        }
        for name in &wave {
            let visit = runs.entry(name.clone()).or_insert(0);
            *visit += 1;
            raw(GraphEvent::NodeStarted {
                workflow: graph.name.clone(),
                node: name.clone(),
                visit: *visit,
            });
        }

        let results = run_wave(&graph, &state, &raw, &wave).await;

        let mut jumps: Vec<String> = Vec::new();
        let mut wave_writes: BTreeMap<String, String> = BTreeMap::new();
        // Merge in wave order — deterministic.
        for (name, out, error, ms) in results {
            path.push(name.clone());
            let keys: Vec<String> = match &out {
                Some(writes) => writes
                    .keys()
                    .filter(|k| !k.starts_with('_'))
                    .cloned()
                    .collect(),
                None => Vec::new(),
            };
            if let Some(writes) = &out {
                for key in &keys {
                    if let Some(first) = wave_writes.get(key) {
                        if first != &name {
                            return Err(GraphError::Collision {
                                key: key.clone(),
                                nodes: vec![first.clone(), name.clone()],
                            });
                        }
                    }
                    wave_writes.insert(key.clone(), name.clone());
                    state.set(key, writes[key].clone());
                }
            }
            raw(GraphEvent::NodeEnded {
                workflow: graph.name.clone(),
                node: name.clone(),
                ms,
                keys,
                error: error.clone(),
            });

            if let Some(message) = error {
                errors.insert(name.clone(), message);
                if let Some(jump) = graph.get(&name).and_then(|n| n.on_error.clone()) {
                    jumps.push(jump);
                }
                continue; // No on_error → nothing fires; the run drains naturally to END
            }

            match graph.routers.get(&name) {
                Some(router) => {
                    let label = (router.route)(&state);
                    let target = router.targets.get(&label).cloned();
                    raw(GraphEvent::Route {
                        workflow: graph.name.clone(),
                        router: name.clone(),
                        target: target.clone().unwrap_or_else(|| END.to_string()),
                        reason: label.clone(),
                    });
                    match target {
                        None => {
                            errors.insert(
                                name.clone(),
                                format!("router returned unknown label '{label}'"),
                            );
                        }
                        Some(target) if target != END => jumps.push(target), // a route is a jump, not a dependency
                        Some(_) => {}
                    }
                }
                None => {
                    for edge in &graph.edges {
                        if edge.src == name && edge.dst != END {
                            fired
                                .entry(edge.dst.clone())
                                .or_default()
                                .insert(name.clone());
                        }
                    }
                }
            }
        }
        wave = next_wave(&graph, &deps, &fired, &runs, &jumps, &mut errors);
    }

    let ms = elapsed_ms(t0);
    let first_error = errors.values().next().cloned();
    raw(GraphEvent::Ended {
        workflow: graph.name.clone(),
        ms,
        steps: path.len() as i32,
        path: path.clone(),
        error: first_error,
    });
    // Errors are also written back into state — callers can see them by reading state.
    state.set(
        "errors",
        Value::Object(
            errors
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect(),
        ),
    );
    Ok(RunReport {
        state,
        path,
        errors,
        ms,
    })
}

/// Runs one wave of nodes. Several run concurrently — they only see the same
/// snapshot, so order doesn't matter (writes are merged in wave order later).
async fn run_wave(
    graph: &Graph,
    state: &State,
    raw: &Observer,
    wave: &[String],
) -> Vec<(String, Option<NodeWrites>, Option<String>, i64)> {
    let mut futures = Vec::with_capacity(wave.len());
    for name in wave {
        let node = match graph.get(name) {
            Some(node) => node,
            None => continue, // next_wave only admits names in the graph; a miss here is a bug
        };
        let f = node.f.clone();
        let node_name = name.clone();
        // One node=-tagged outlet per node: nodes needn't know their name in the graph.
        let tagged = raw.clone();
        let inner: LoopObserver = Arc::new(move |event| {
            tagged(GraphEvent::Inner {
                node: node_name.clone(),
                event,
            })
        });
        let ctx = NodeCtx {
            state: state.clone(),
            inner,
        };
        futures.push(async move {
            let t = Instant::now();
            let out = f(ctx).await;
            match out {
                Ok(writes) => (name.clone(), Some(writes), None, elapsed_ms(t)),
                Err(message) => (name.clone(), None, Some(message), elapsed_ms(t)),
            }
        });
    }
    join_all(futures).await
}

/// Nodes with forced jumps (routes / error detours) plus nodes whose static in-edges
/// have all fired. Note this is **deterministic**: jumps first, then a scan in
/// insertion order; no duplicates within one wave.
fn next_wave(
    graph: &Graph,
    deps: &BTreeMap<String, BTreeSet<String>>,
    fired: &BTreeMap<String, BTreeSet<String>>,
    runs: &BTreeMap<String, i32>,
    jumps: &[String],
    errors: &mut BTreeMap<String, String>,
) -> Vec<String> {
    let mut wave: Vec<String> = Vec::new();
    let ready = graph.nodes.iter().filter(|n| {
        let empty = BTreeSet::new();
        let deps_n = deps.get(&n.name).unwrap_or(&empty);
        let fired_n = fired.get(&n.name).unwrap_or(&empty);
        !deps_n.is_empty()
            && deps_n.is_subset(fired_n)
            && runs.get(&n.name).copied().unwrap_or(0) == 0
    });
    for name in jumps
        .iter()
        .map(String::as_str)
        .chain(ready.map(|n| n.name.as_str()))
    {
        if name == END || wave.iter().any(|w| w == name) {
            continue;
        }
        let node = match graph.get(name) {
            Some(node) => node,
            None => continue,
        };
        if runs.get(name).copied().unwrap_or(0) >= node.max_visits {
            errors
                .entry(name.to_string())
                .or_insert_with(|| format!("max_visits={} reached", node.max_visits));
            continue;
        }
        wave.push(name.to_string());
    }
    wave
}

fn existing_errors(state: &State) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(Value::Object(map)) = state.get("errors") {
        for (key, value) in map {
            if let Some(message) = value.as_str() {
                out.insert(key.clone(), message.to_string());
            }
        }
    }
    out
}

fn elapsed_ms(since: Instant) -> i64 {
    since.elapsed().as_millis() as i64
}

pub mod nodes;
pub mod workflows;

#[cfg(test)]
#[path = "graph_tests.rs"]
mod graph_tests;
