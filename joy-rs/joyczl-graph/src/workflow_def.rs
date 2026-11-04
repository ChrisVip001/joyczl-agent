//! Declarative workflow definitions: a JSON DAG, parsed and validated at load.
//!
//! The split of responsibilities is the point:
//! - **JSON declares topology** — node names/kinds, edges, routers with their targets;
//! - **Rust owns behaviour** — node implementations are closures **bound by node name**
//!   at build time (the same injection pattern `build_triage_graph` always used), and a
//!   router may only reference a **registered built-in route function**. Control flow
//!   never goes to the model — that is the standing invariant, now enforced by the loader:
//!   a JSON that names an unknown router simply refuses to build.
//!
//! Structural validation at load: unknown nodes/edges/targets reuse the engine's own
//! build-time errors; **acyclicity is checked over the static edges** by topological
//! sort. Router jumps are exempt — they are conditional, and intentional loops ride on
//! `max_visits` (the runtime guard), which the JSON can declare per node.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use crate::nodes::key_router;
use crate::{Node, NodeFn, RouteFn, State};

/// A named builder for a route function: `name → params → RouteFn`. The registry is
/// closed — anything not in it cannot steer control flow.
pub type RouteBuilders = BTreeMap<String, fn(&Value) -> Result<RouteFn, String>>;

/// The built-in route functions. This list **is** the whitelist.
pub fn builtin_routes() -> RouteBuilders {
    let mut routes: RouteBuilders = BTreeMap::new();
    routes.insert("key_router".to_string(), |params| {
        let key = params
            .get("key")
            .and_then(Value::as_str)
            .ok_or("key_router 需要 params.key")?;
        let default = params
            .get("default")
            .and_then(Value::as_str)
            .ok_or("key_router 需要 params.default")?;
        Ok(key_router(key.to_string(), default.to_string()))
    });
    routes.insert("needs_action".to_string(), |_| {
        Ok(Arc::new(crate::nodes::needs_action))
    });
    routes
}

/// One node declaration. `kind` stays a label (the engine doesn't dispatch on it —
/// behaviour comes from the binding); `max_visits` > 1 marks an intentional loop.
#[derive(Debug, Deserialize)]
pub struct NodeDef {
    pub name: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default = "default_visits")]
    pub max_visits: i32,
    #[serde(default)]
    pub on_error: Option<String>,
}

fn default_kind() -> String {
    "fn".to_string()
}
fn default_visits() -> i32 {
    1
}

/// A conditional jump table hanging on one node.
#[derive(Debug, Deserialize)]
pub struct RouterDef {
    pub src: String,
    /// A **registered built-in** — the loader refuses unknown names.
    pub router: String,
    #[serde(default)]
    pub params: Value,
    /// label → target node name (or `"END"`).
    #[serde(default)]
    pub targets: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowDef {
    pub name: String,
    pub nodes: Vec<NodeDef>,
    /// START → these nodes (same as `Graph::entry`).
    #[serde(default)]
    pub entry: Vec<String>,
    /// Unconditional static edges. These are what the acyclicity check walks.
    #[serde(default)]
    pub edges: Vec<(String, String)>,
    #[serde(default)]
    pub routers: Vec<RouterDef>,
}

impl WorkflowDef {
    pub fn parse(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| format!("工作流 JSON 解析失败：{e}"))
    }

    /// Build a runnable `Graph`. `bindings` must cover **every** node name — a JSON
    /// node without a bound implementation is a load error, not a silent noop.
    pub fn build(
        &self,
        bindings: &BTreeMap<String, NodeFn>,
        routes: &RouteBuilders,
    ) -> Result<crate::Graph, String> {
        self.check_acyclic()?;
        let mut graph = crate::Graph::new(&self.name);
        for def in &self.nodes {
            let f = bindings.get(&def.name).ok_or_else(|| {
                format!(
                    "节点 '{}' 没有绑定的实现 —— JSON 只声明拓扑，行为来自按名注入的代码",
                    def.name
                )
            })?;
            let mut node = Node::new(&def.name, &def.kind, f.clone());
            node.max_visits = def.max_visits;
            node.on_error = def.on_error.clone();
            graph
                .add_node(node)
                .map_err(|e| format!("节点 '{}'：{e}", def.name))?;
        }
        for (src, dst) in &self.edges {
            graph
                .add_edge(src, dst)
                .map_err(|e| format!("边 {src}→{dst}：{e}"))?;
        }
        for router in &self.routers {
            let builder = routes.get(&router.router).ok_or_else(|| {
                format!(
                    "路由 '{}' 未注册 —— 控制流只能交给内置路由函数（绝不交给模型），\
                     可用：{:?}",
                    router.router,
                    routes.keys().collect::<Vec<_>>()
                )
            })?;
            let route = builder(&router.params)
                .map_err(|e| format!("路由 '{}' 的参数错误：{e}", router.router))?;
            let targets: Vec<(&str, &str)> = router
                .targets
                .iter()
                .map(|(label, dst)| (label.as_str(), dst.as_str()))
                .collect();
            graph
                .add_router(&router.src, route, &targets)
                .map_err(|e| format!("路由 '{}'：{e}", router.src))?;
        }
        if !self.entry.is_empty() {
            let names: Vec<&str> = self.entry.iter().map(String::as_str).collect();
            graph
                .entry(&names)
                .map_err(|e| format!("入口声明错误：{e}"))?;
        }
        Ok(graph)
    }

    /// Topological sort over the **static edges** — a cycle there is a definition bug
    /// (a parallel wave can't decide order); intentional loops must be expressed as
    /// routers + `max_visits`, which this check deliberately does not walk.
    fn check_acyclic(&self) -> Result<(), String> {
        let mut indegree: BTreeMap<&str, usize> = BTreeMap::new();
        let mut adjacency: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for def in &self.nodes {
            indegree.entry(def.name.as_str()).or_insert(0);
        }
        for (src, dst) in &self.edges {
            adjacency.entry(src).or_default().push(dst);
            *indegree.entry(dst).or_insert(0) += 1;
        }
        let mut queue: VecDeque<&str> = indegree
            .iter()
            .filter(|(_, &degree)| degree == 0)
            .map(|(&name, _)| name)
            .collect();
        let mut seen = 0usize;
        while let Some(name) = queue.pop_front() {
            seen += 1;
            if let Some(nexts) = adjacency.get(name) {
                for next in nexts {
                    let degree = indegree.get_mut(next).expect("边端点已登记");
                    *degree -= 1;
                    if *degree == 0 {
                        queue.push_back(next);
                    }
                }
            }
        }
        if seen != indegree.len() {
            let stuck: Vec<&str> = indegree
                .iter()
                .filter(|(_, &degree)| degree > 0)
                .map(|(&name, _)| name)
                .collect();
            return Err(format!(
                "工作流 '{}' 的静态边有环（{stuck:?}）—— 并行波需要无环结构；\
                 有意的循环请用路由跳转 + max_visits 表达",
                self.name
            ));
        }
        Ok(())
    }
}

/// Re-exported so workflows can build bindings maps without importing two modules.
pub fn bind(name: &str, f: NodeFn) -> (String, NodeFn) {
    (name.to_string(), f)
}

/// The `State` a router reads; kept here so the route registry's signature has a home.
pub type RouteInput<'a> = &'a State;
