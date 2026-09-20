//! gather 图的测试。
//!
//! 全部用脚本化的桩 —— 模型在这层不存在（synthesize 的桩就是一段字符串）。
//! 钉三件事：propose 路径走到 draft_digest、quiet 路径提前收工、
//! 单个 scan 挂掉时用诚实文字兜底而不是让整趟空转。

use std::sync::{Arc, Mutex};

use super::{
    build_gather_graph, gather_topology, needs_action, scan_owns, synth_request, CalendarScanFn,
    DraftFn, GithubScanFn, MemoryScanFn, SynthFn, WebScanFn,
};
use crate::{run_graph, Boxed, State, DEFAULT_MAX_STEPS};

/// 把一个现成的结果包成 scan 的签名 —— 测试里最常用的桩。
fn scan<T: Send + 'static>(value: Result<T, String>) -> Boxed<T> {
    Box::pin(async move { value })
}

/// 有事发生：PR 与日历都有数 —— 应当走 draft_digest。
#[tokio::test]
async fn with_pending_work_it_drafts_a_digest() {
    let github: GithubScanFn = Arc::new(|| scan(Ok(("Open PRs:\n- #12 fix loop".into(), 1, 0))));
    let web: WebScanFn = Arc::new(|| scan(Ok("(web results)".into())));
    let calendar: CalendarScanFn = Arc::new(|| scan(Ok(("standup".into(), 1))));
    let memory: MemoryScanFn = Arc::new(|| scan(Ok("alex prefers mornings".into())));
    let digest = Arc::new(Mutex::new(String::new()));
    let synth_digest = digest.clone();
    let synth: SynthFn = Arc::new(move |state| {
        let digest = synth_digest.clone();
        Box::pin(async move {
            // prompt 真的把四个 scan 的产出缝进去了。
            assert!(state.str("gh_text").unwrap().contains("#12"));
            let text = "DIGEST: 今天只有一件事。";
            *digest.lock().unwrap() = text.to_string();
            Ok(text.to_string())
        })
    });
    let drafted = Arc::new(Mutex::new(false));
    let draft_flag = drafted.clone();
    let draft: DraftFn = Arc::new(move |state| {
        let flag = draft_flag.clone();
        Box::pin(async move {
            assert!(state.str("digest").unwrap().starts_with("DIGEST"));
            *flag.lock().unwrap() = true;
            Ok("/outbox/gather-2026-09-20.md".to_string())
        })
    });

    let graph = build_gather_graph(github, web, calendar, memory, synth, draft).expect("建图");
    let report = run_graph(graph, State::new(), None, DEFAULT_MAX_STEPS)
        .await
        .expect("跑图");

    assert!(
        report.path.contains(&"draft_digest".to_string()),
        "有事发生就该写草稿：{:?}",
        report.path
    );
    assert!(*drafted.lock().unwrap(), "draft 节点真的跑过");
    assert_eq!(
        report.state.str("draft_path").map(str::to_string),
        Some("/outbox/gather-2026-09-20.md".to_string())
    );
    assert!(!digest.lock().unwrap().is_empty());
}

/// 全部安静：路由器走 quiet，draft_digest 根本不跑 ——
/// 「没东西就别打扰人」是路由器的职责，不是模型的。
#[tokio::test]
async fn a_quiet_day_ends_before_the_draft() {
    let github: GithubScanFn = Arc::new(|| scan(Ok((String::new(), 0, 0))));
    let web: WebScanFn = Arc::new(|| scan(Ok(String::new())));
    let calendar: CalendarScanFn = Arc::new(|| scan(Ok((String::new(), 0))));
    let memory: MemoryScanFn = Arc::new(|| scan(Ok(String::new())));
    let synth: SynthFn = Arc::new(|_s| Box::pin(async { Ok("（都安静。）".to_string()) }));
    let draft: DraftFn = Arc::new(|_s| {
        Box::pin(async {
            panic!("quiet 路径不该跑 draft");
            #[allow(unreachable_code)]
            Ok(String::new())
        })
    });

    let graph = build_gather_graph(github, web, calendar, memory, synth, draft).expect("建图");
    let report = run_graph(graph, State::new(), None, DEFAULT_MAX_STEPS)
        .await
        .expect("跑图");

    assert!(
        !report.path.contains(&"draft_digest".to_string()),
        "{:?}",
        report.path
    );
    assert!(report.state.str("draft_path").is_none());
    assert_eq!(report.state.str("digest"), Some("（都安静。）"));
}

/// 单个 scan 挂掉：它的键变成诚实的 "unavailable (…)" 文字，
/// 整张图照常出摘要 —— 沉默是最坏的失败方式。
#[tokio::test]
async fn a_failed_scan_becomes_honest_text_not_silence() {
    let github: GithubScanFn = Arc::new(|| scan(Err("gh CLI 超时".to_string())));
    let web: WebScanFn = Arc::new(|| scan(Ok("(web results)".into())));
    let calendar: CalendarScanFn = Arc::new(|| scan(Ok((String::new(), 0))));
    let memory: MemoryScanFn = Arc::new(|| scan(Ok(String::new())));
    let synth: SynthFn = Arc::new(|state| {
        Box::pin(async move {
            // 失败的 scan 以文字进 prompt —— 模型会照实说「那部分没拿到」。
            assert!(
                state.str("gh_text").unwrap().contains("unavailable"),
                "{:?}",
                state.str("gh_text")
            );
            Ok("尽力而为的晨报。".to_string())
        })
    });
    let draft: DraftFn = Arc::new(|_s| Box::pin(async { Ok("outbox/gather.md".to_string()) }));

    let graph = build_gather_graph(github, web, calendar, memory, synth, draft).expect("建图");
    let report = run_graph(graph, State::new(), None, DEFAULT_MAX_STEPS)
        .await
        .expect("跑图");
    assert!(
        report.errors.is_empty(),
        "scan 的失败不该留下引擎级错误：{:?}",
        report.errors
    );
    assert_eq!(report.state.str("digest"), Some("尽力而为的晨报。"));
}

/// 路由器直接可测：计数进，标签出 —— 不需要模型，也不需要图。
#[test]
fn the_router_reads_counts_not_prose() {
    let mut state = State::new();
    assert_eq!(needs_action(&state), "quiet");
    state.set("gh_open_prs", serde_json::json!(2));
    assert_eq!(needs_action(&state), "propose");

    let mut state = State::new();
    state.set("cal_event_count", serde_json::json!(1));
    assert_eq!(needs_action(&state), "propose");
}

/// 拓扑（dashboard 画图的数据）：节点齐全，四个 scan 都汇进 synthesize。
#[test]
fn the_topology_describes_the_shape() {
    let topology = gather_topology();
    assert_eq!(topology.name, "gather");
    let names: Vec<&str> = topology.nodes.iter().map(|n| n.name.as_str()).collect();
    for expected in [
        "scan_github",
        "scan_web",
        "scan_calendar",
        "scan_memory",
        "synthesize",
        "draft_digest",
    ] {
        assert!(names.contains(&expected), "{names:?}");
    }
}

/// 每个 scan 只写自己名下的键 —— 这条约定拿这份清单核对。
#[test]
fn scan_ownership_lists_are_disjoint() {
    let mut seen: Vec<&str> = Vec::new();
    for keys in [
        scan_owns("scan_github"),
        scan_owns("scan_web"),
        scan_owns("scan_calendar"),
        scan_owns("scan_memory"),
    ] {
        for key in keys {
            assert!(!seen.contains(key), "'{key}' 被两个 scan 认领");
            seen.push(key);
        }
    }
}

/// synthesize 的请求里没有 tools —— 「只提议，绝不行动」是结构保证，
/// 不是 prompt 里的一句嘱咐。
#[test]
fn the_synth_request_carries_no_tools() {
    let mut state = State::new();
    state.set("gh_text", serde_json::json!("PR #1"));
    state.set("web_text", serde_json::json!(""));
    state.set("cal_text", serde_json::json!("standup"));
    state.set("mem_text", serde_json::json!(""));
    let request = synth_request("test-model", &state);
    assert!(request.tools.is_empty(), "tools 必须是空的");
    assert!(request.messages[0].text().contains("PR #1"));
    assert!(request.messages[0].text().contains("standup"));
}
