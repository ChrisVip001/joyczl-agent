//! 节点的常见形状。
//!
//! 一切本来就是 `NodeFn`（state 进去，要合并的键出来），`Node::new` 就能包
//! 住任何闭包 —— 专门的「包装器」只是把调用变长了一行。
//!
//! 真正值得留下的是 `agent_node` 想说的那件事：一个节点可以是**一整轮
//! loop**。图不替代 loop —— 它安排 loop 被调用的位置，以及它周围还发生
//! 什么。triage 的 `full_agent` 就是这么接的（见 `workflows::triage`），
//! 而且它调用的就是不带图时同一个 `full_turn`，所以「loop 当节点」和
//! 「loop 当默认」永远不会走散。
//!
//! 同理没留下的还有 `llm_node`：它把 prompt 模板和 state 用 `format` 缝在
//! 一起，而需要它的两个地方（triage 的 classify、quick_reply）都得自己解
//! 析返回值或自己挑模型，手写反而更短。

use std::sync::Arc;

use crate::{NodeCtx, NodeFn, NodeWrites, RouteFn, State};

/// 本家路由器：读 state 里的一个键，把它当作路由标签返回
/// （键不存在或是空字符串就用 `default`）。模型写这个键，这段代码读它。
pub fn key_router(key: &'static str, default: &'static str) -> RouteFn {
    Arc::new(move |state: &State| match state.str(key) {
        Some(value) if !value.is_empty() => value.to_string(),
        _ => default.to_string(),
    })
}

/// 逃生口：任何 `state 进 → 键出来` 的**纯**函数都能直接当节点，
/// 不用手写 `Box::pin`。读文件、查库、算数这类同步活儿就用它。
/// 要 await（调模型、调工具）的节点还得自己装箱 —— 那时候装箱是必须的，
/// 不是仪式。
pub fn fn_node<F>(f: F) -> NodeFn
where
    F: Fn(&State) -> NodeWrites + Send + Sync + 'static,
{
    Arc::new(move |ctx: NodeCtx| {
        let out = f(&ctx.state);
        Box::pin(async move { Ok(out) })
    })
}
