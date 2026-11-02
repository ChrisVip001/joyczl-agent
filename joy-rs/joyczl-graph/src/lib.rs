//! THE GRAPH —— 节点、边、波次。这个文件就是全部机制。
//!
//! 图工作流是一张步骤地图：每个节点干一件事，每条边说「这件事干完了就去哪」。
//! 引擎按波次跑它：
//!
//! ```text
//! while 还有节点就绪:
//!     跑这一波所有就绪的节点（有多个就同时跑）
//!     把它们写的东西并进共享 state
//!     触发它们的边 / 问它们的 router 下一步去哪
//! ```
//!
//! 就这些。三件事撑起全部：
//!
//!   state    一个普通的 map（黑板）。每个节点读它，返回自己想并进去的键。
//!            并行的节点必须写**不相交**的键 —— 撞了就直接报错，而不是悄悄丢一次写。
//!   routers  对 state 的普通函数。模型写 state，代码读它并选边。
//!            控制流永远不由模型直接决定。
//!   guards   loop 那套双护栏的推广：每节点 max_visits（有界循环）+
//!            全局 max_steps（绝不空转）。节点出错如实记下并露出来，
//!            绝不从 run 里抛出去 —— 跟 ToolRegistry.execute 同一条
//!            「露出来，别崩」的规矩。
//!
//! 波次用一点流水线换回大量可读性：执行顺序是确定的，于是 trace 读两遍
//! 都一样，测试也能钉死路径。
//!
//! observer 是一个枚举事件而不是 `(kind, dict)` —— 跟 loop 同一个理由，
//! 字段有了类型就拼不错。observer 本身就是 Sync，锁由落盘那一侧自己拿。

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

/// 图的起点：一个虚拟节点，只负责点着第一批节点。
pub const START: &str = "START";
/// 图的终点：走到这里就收工，不产生任何节点。
pub const END: &str = "END";

/// 默认全局步数上限：到顶说明图没收敛。
pub const DEFAULT_MAX_STEPS: usize = 25;

/// 节点写回的键值对。
pub type NodeWrites = Map<String, Value>;

/// 节点函数的返回：要么写回一批键，要么说明为什么不行。
pub type Boxed<T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send>>;

/// 节点拿到的东西：一份 state 快照 + 一个出口。
pub struct NodeCtx {
    /// 黑板的快照。节点改它不影响别人，想留下的东西要**返回**回引擎。
    pub state: State,
    /// 节点内部的事件（loop 的 llm / text / tool）从这里出去，
    /// 引擎负责补上 node= 再转给图的观察者。
    pub inner: LoopObserver,
}

pub type NodeFn = Arc<dyn Fn(NodeCtx) -> Boxed<NodeWrites> + Send + Sync>;

/// 路由函数：读 state 的**当前**内容，返回一个边的标签。
/// 它是代码，不是模型。
pub type RouteFn = Arc<dyn Fn(&State) -> String + Send + Sync>;

/// 图的观察者。跟 loop 一样，用 Arc 是为了能搬进 'static 闭包。
pub type Observer = Arc<dyn Fn(GraphEvent) + Send + Sync>;

/// graph 往外发的事件。跟 loop 同一套规矩：gateway 拿它画界面，
/// trace 拿它落盘，两者都不需要被接进图自己的逻辑里。
#[derive(Debug, Clone, PartialEq)]
pub enum GraphEvent {
    Started {
        workflow: String,
        nodes: Vec<String>,
    },
    NodeStarted {
        workflow: String,
        node: String,
        /// 这是本节点第几次跑（>1 说明它在循环里）。
        visit: i32,
    },
    NodeEnded {
        workflow: String,
        node: String,
        ms: i64,
        /// 本次写回的键（不含下划线开头的私有键）。
        keys: Vec<String>,
        error: Option<String>,
    },
    Route {
        workflow: String,
        router: String,
        /// 走到的节点；router 返回了不认识的标签时是 END。
        target: String,
        /// router 返回的标签本身。
        reason: String,
    },
    /// 节点自己发的内部事件（full_agent 里的 llm / text / tool），
    /// 引擎补上 node= 再转出来。
    Inner { node: String, event: LoopEvent },
    Ended {
        workflow: String,
        ms: i64,
        steps: i32,
        path: Vec<String>,
        error: Option<String>,
    },
}

/// 黑板。节点读它、写它，路由器读它。
///
/// 实现就是一个 `BTreeMap<String, Value>`：键有序，于是并行的写合并进
/// state 后顺序也是确定的（trace 与测试都受益）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State(Map<String, Value>);

impl State {
    pub fn new() -> Self {
        State(Map::new())
    }

    /// 从 JSON 对象建 state；不是对象就是调用方的错。
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

    /// 反序列化某个键。类型不对就是节点写坏了东西 —— 报错，别猜。
    pub fn get_as<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
        serde_json::from_value(self.0.get(key)?.clone()).ok()
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// 并进一批写回。下划线开头的键是节点/引擎私有的，不出图。
    pub fn merge(&mut self, writes: NodeWrites) {
        for (key, value) in writes {
            if !key.starts_with('_') {
                self.0.insert(key, value);
            }
        }
    }
}

/// 写回一批键值对。节点函数的出口用这个，省得手搓 Map。
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

/// 图里的一个节点。
pub struct Node {
    pub name: String,
    /// tool / llm / agent / router —— 给 describe() 用的标签。
    pub kind: String,
    /// >1 只用在**有意**的循环里的节点上。
    pub max_visits: i32,
    /// 出错时跳到哪。不设就是空转到 END（跟 loop 的护栏同一条规矩）。
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

/// 一条无条件的边。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Edge {
    pub src: String,
    pub dst: String,
}

/// 拓扑里的一条边（多一个 conditional 标记）—— dashboard 画图用的数据。
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

/// 拓扑即数据。界面从这份数据画图（而不是照着一张手抄的图片），
/// 图才不会跟代码走散。
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

/// 图本身：节点、边、路由。建好之后只读。
pub struct Graph {
    name: String,
    /// 用一个 Vec + 索引表保存节点：**插入顺序**对波次计算是语义的一部分。
    nodes: Vec<Node>,
    index: BTreeMap<String, usize>,
    edges: Vec<Edge>,
    routers: BTreeMap<String, Router>,
}

/// 图的结构性错误（建图时就能发现，或者同一波次里两个节点撞了键）。
#[derive(Debug, Clone, PartialEq)]
pub enum GraphError {
    /// START / END 是保留名。
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
    /// 同一波次里两个节点写了同一个键 —— 这是图的 bug，不是竞态。
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

    /// 无条件的边：src 干完了，dst 就离就绪近一步。
    /// 先加节点再加边 —— 未知端点在这里就报错，而不是跑到一半才发现。
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

    /// 有条件的边：src 干完了，`route(state)` 返回一个标签，
    /// 执行就跳到 targets[标签]。路由器是代码，永远不是模型。
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

    /// 起点：START → 这批节点。等价于一批 `add_edge(START, ..)`，
    /// 但读起来就是「图从哪开始」。
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

    /// 节点名，按插入顺序。
    pub fn node_names(&self) -> Vec<String> {
        self.nodes.iter().map(|n| n.name.clone()).collect()
    }

    /// 每个节点的静态入边。END 没有入边（走到 END 就是收工）。
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

    /// 拓扑即数据。
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
            // dict.fromkeys：同一个目标只画一条边。
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

/// 一次图运行的收成。
#[derive(Debug, Clone)]
pub struct RunReport {
    /// 跑完之后的 state。`result` / `reply` 这类键就是图的产出。
    pub state: State,
    /// 执行路径，按实际顺序 —— 测试钉的就是这个。
    pub path: Vec<String>,
    /// 每个节点自己的错，外加引擎自己的（`engine` 这个 key）。
    pub errors: BTreeMap<String, String>,
    pub ms: i64,
}

impl RunReport {
    /// 第一个错（`errors` 是 BTreeMap，顺序是键序 —— 跟 Python 的插入序
    /// 不同，但它的用途只是「有没有出错、错的是什么」，不是顺序）。
    pub fn first_error(&self) -> Option<&str> {
        self.errors.values().next().map(String::as_str)
    }
}

/// 跑完一张图。返回最终的 state；错误落在 `state["errors"]` 和
/// `graph_end` 事件里，绝不抛出来。
///
/// 唯一的例外是节点撞键（[`GraphError::Collision`]）—— 那是图的 bug，
/// 静默丢一次写比报错糟得多。
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
    // 上一次跑留下的错要留着（state 可以是复用的）。
    let mut errors: BTreeMap<String, String> = existing_errors(&state);

    let mut state = state;
    raw(GraphEvent::Started {
        workflow: graph.name.clone(),
        nodes: graph.node_names(),
    });

    // START 先把自己的边点着，再算第一波。
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
        // 按波次顺序合并 —— 确定的。
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
                continue; // 没有 on_error → 什么都不触发，运行自然地排空到 END
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
                        Some(target) if target != END => jumps.push(target), // 路由是跳，不是依赖
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
    // 错也如实写回 state —— 调用方读 state 就能看到。
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

/// 跑一波节点。有多个就同时跑 —— 它们只看得到同一份快照，
/// 所以谁先谁后都行（写回是后面按波次顺序合并的）。
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
            None => continue, // next_wave 只放图里有的名字，这里取不到是 bug
        };
        let f = node.f.clone();
        let node_name = name.clone();
        // 每个节点一个已打好 node= 标签的出口：节点不用知道自己在图里的名字。
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

/// 强制跳转（路由 / 出错改道）的节点 + 静态入边全部点着的节点。
///
/// 注意这是**确定性**的：先跳转，再按插入顺序扫；同一波里不会重复。
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
