//! gather 图工作流 —— 晨报，真正配得上「图」的那个工作流。
//!
//! 每天早上都是同样四个问题：仓库里开着什么、外面在说什么、今天排了什么、
//! 我已经知道什么。这张单子不是发现的 —— 醒来之前就知道。让模型一个工具
//! 一个工具地摸出来什么也买不到，还多花四个来回，赶上状态差还会漏一个。
//!
//! ```text
//! START ─┬─ scan_github  ─┐
//!        ├─ scan_web      │
//!        ├─ scan_calendar ├─► synthesize ──router──► draft_digest → END
//!        └─ scan_memory  ─┘                    └──── quiet ──────► END
//! ```
//!
//! 四个 scan 互相不依赖，引擎把它们放进同一波并行跑 —— 这就是图在这里的
//! 全部论证：形状事先知道，所以画得出来；画出来之后，独立的部分一眼独立。
//!
//! 本文件遵守的两条铁律（都是承重墙）：
//!
//! 1. **只提议，绝不行动。** 图里没有 agent 节点、没有 loop、没有
//!    ToolRegistry。唯一的模型调用是一次裸 `create`，**不带 tools 参数**
//!    —— 模型手里根本没有可用来发送、合并、创建任何东西的 schema。
//!    不是「被嘱咐别做」，是「做不了」。全图唯一的写，是 outbox 里
//!    一个给人看的 markdown 文件。
//!
//! 2. **每条分支自己兜住自己的失败。** 抛错的节点不触发任何边，
//!    synthesize 的依赖就永远凑不齐，整趟跑下来什么都没有 —— 没报错、
//!    没晨报、空 outbox。对晨报来说，沉默是最坏的失败方式，所以每个
//!    scan 失败时返回**诚实的文字**（"unavailable (…)"）而不是报错。
//!    见下面每个节点的 safe 包装。

use std::sync::Arc;

use joyczl_provider::{CreateRequest, Message};
use serde_json::json;

use crate::{writes, Boxed, Graph, GraphError, Node, NodeCtx, NodeWrites, State, Topology, END};

/// 每个 scan 只写带自己前缀的键。并行节点写同一个键会触发引擎的
/// GraphStateCollision —— 那是兜底；让审阅者一眼能核对的约定才是防线。
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

// ---- 注入的可调用体 ---------------------------------------------------------
//
// 跟 triage 同一套做法：测试用脚本化的桩，gather_topology() 用桩建图只为
// describe()，CLI 把真实现绑上来。scan 是 async 的（真实现里它们要跑子进程
// 和 HTTP）；`Boxed<T>` 的 Err(String) 就是失败原因 —— 节点把它翻译成
// 诚实的文字，见 `unavailable`。

pub type GithubScanFn = Arc<dyn Fn() -> Boxed<(String, i64, i64)> + Send + Sync>;
pub type WebScanFn = Arc<dyn Fn() -> Boxed<String> + Send + Sync>;
pub type CalendarScanFn = Arc<dyn Fn() -> Boxed<(String, i64)> + Send + Sync>;
pub type MemoryScanFn = Arc<dyn Fn() -> Boxed<String> + Send + Sync>;
pub type SynthFn = Arc<dyn Fn(State) -> Boxed<String> + Send + Sync>;
pub type DraftFn = Arc<dyn Fn(State) -> Boxed<String> + Send + Sync>;

/// 路由器：对 scan 写回的**计数**做判断 —— 永远不对摘要的散文措辞做路由。
///
/// 引擎的规矩是「路由器是代码，不是模型」。拿模型的措辞当路由条件，等于把
/// 控制流交给模型，而且每条分支都得配一段脚本应答才能测。计数是精确的、
/// 便宜的，测试可以直接驱动。
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

/// scan 失败时的诚实文案 —— 「unavailable」本身就是信息，比沉默强。
fn unavailable(why: &str) -> String {
    format!("unavailable ({why})")
}

/// scan 节点的通用形状：跑一个 async scan，Ok → 成功的键值，
/// Err → 失败的键值（诚实文案 / 归零）。**每个 scan 自己兜住自己的失败**
/// 这条规矩，模板只写一遍。
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

/// 单文本 scan：结果（或 unavailable 文案）写进一个键。
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

    // 唯一的模型调用，**没有 tools**。见模块注释里的铁律 1。
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

    // 全图唯一的写，而且是一个给人打开的文件。
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

    // 四个 scan 互不依赖 → 同一波并行；synthesize 等全部四个到齐。
    for scan in ["scan_github", "scan_web", "scan_calendar", "scan_memory"] {
        g.entry(&[scan])?;
        g.add_edge(scan, "synthesize")?;
    }

    // 路由器挂在 synthesize 上：它就是那道汇合的墙，triage 里那个
    // 单独的 gather 节点在这儿是死重。
    g.add_router(
        "synthesize",
        Arc::new(needs_action),
        &[("propose", "draft_digest"), ("quiet", END)],
    )?;
    g.add_edge("draft_digest", END)?;

    Ok(g)
}

/// 拓扑即数据，给 dashboard 用 —— 用桩建图，从不运行。
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
        // 走到这里说明图本身写错了，而那张图是写死在这个文件里的。
        Err(error) => panic!("gather 图写错了：{error}"),
    }
}

/// 用真 provider 拼一次 synthesize 的请求 —— CLI 的绑定用它，
/// 保持着「无 tools」那条铁律的最后一道关口。
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

/// 供测试断言「并行节点写不相交的键」用的清单 ——
/// 每个节点名和它允许写的键前缀。
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
