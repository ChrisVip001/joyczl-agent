//! Intra-turn budget rules: big ones only, largest first, idempotent, self-budgeted untouched.

use joyczl_provider::{ContentBlock, Message, Role};

use super::budget::{trim_tool_results, ToolResultBudget};

fn call(id: &str, name: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input: serde_json::json!({}),
            extra: None,
        }],
    }
}

fn result(id: &str, chars: usize) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: "x".repeat(chars),
        }],
    }
}

fn budget(total: usize, per_result: usize) -> ToolResultBudget {
    ToolResultBudget {
        total_chars: total,
        per_result_chars: per_result,
    }
}

fn result_text(messages: &[Message], index: usize) -> &str {
    match &messages[index].content[0] {
        ContentBlock::ToolResult { content, .. } => content,
        _ => panic!("不是工具结果"),
    }
}

#[test]
fn a_disabled_budget_does_nothing() {
    let mut messages = vec![call("1", "mcp__x__big"), result("1", 500_000)];
    let before = result_text(&messages, 1).chars().count();

    let report = trim_tool_results(&mut messages, None, &ToolResultBudget::disabled(), "s");

    assert_eq!(report.trimmed, 0);
    assert_eq!(result_text(&messages, 1).chars().count(), before);
}

#[test]
fn a_batch_under_the_cap_is_left_alone() {
    let mut messages = vec![call("1", "mcp__x__small"), result("1", 1_000)];
    let report = trim_tool_results(&mut messages, None, &budget(10_000, 5_000), "s");
    assert_eq!(report.trimmed, 0);
    assert!(!report.still_over_budget);
    assert!(!result_text(&messages, 1).contains("已截断"));
}

#[test]
fn the_largest_results_are_stubbed_until_it_fits() {
    let mut messages = vec![
        call("1", "mcp__x__huge"),
        result("1", 100_000),
        call("2", "mcp__x__big"),
        result("2", 50_000),
        call("3", "mcp__x__small"),
        result("3", 1_000),
    ];

    // Budget 10k: stubbing the largest still leaves us over, so the second goes too — until it fits.
    let report = trim_tool_results(&mut messages, None, &budget(10_000, 30_000), "s");

    assert_eq!(report.trimmed, 2, "两条都得换成桩才装得下：{report:?}");
    assert!(!report.still_over_budget);
    assert!(result_text(&messages, 1).contains("已截断"), "第一条该换掉");
    assert!(result_text(&messages, 3).contains("已截断"), "第二条该换掉");
    // The small one is kept as-is — a file for a thousand chars is not worth it (below the threshold).
    assert_eq!(result_text(&messages, 5), "x".repeat(1_000));
    assert!(report.total_chars <= 10_000, "{report:?}");
}

#[test]
fn it_stops_as_soon_as_the_batch_fits() {
    // Fits, so stop: `if total <= max: break` — once it fits, stop, and do not also stub
    // results that could have stayed.
    let mut messages = vec![
        call("1", "mcp__x__huge"),
        result("1", 100_000),
        call("2", "mcp__x__big"),
        result("2", 50_000),
    ];

    let report = trim_tool_results(&mut messages, None, &budget(60_000, 30_000), "s");

    assert_eq!(report.trimmed, 1, "换掉最大的就够 6 万了：{report:?}");
    assert!(!report.still_over_budget);
    assert_eq!(result_text(&messages, 3).chars().count(), 50_000);
}

#[test]
fn small_results_are_never_stubbed_even_when_the_total_is_over() {
    // 100 results of 1000 chars each = 100k, over the 50k budget; but none reaches the 30k threshold.
    let mut messages = Vec::new();
    for i in 0..100 {
        messages.push(call(&i.to_string(), "mcp__x__chatty"));
        messages.push(result(&i.to_string(), 1_000));
    }

    let report = trim_tool_results(&mut messages, None, &budget(50_000, 30_000), "s");

    assert_eq!(report.trimmed, 0, "给一百条小结果各建一个文件更贵");
    assert!(report.still_over_budget, "要如实承认没达标，不假装处理过了");
    assert!(!result_text(&messages, 1).contains("已截断"));
}

#[test]
fn a_self_budgeted_tool_is_left_to_the_tool_itself() {
    // run_command spills and truncates to 8000 itself; another wrapper just swaps paths around.
    let mut messages = vec![call("1", "run_command"), result("1", 40_000)];
    let report = trim_tool_results(&mut messages, None, &budget(10_000, 5_000), "s");
    assert_eq!(report.trimmed, 0);
    assert!(!result_text(&messages, 1).contains("已截断"));
}

#[test]
fn trimming_is_idempotent() {
    let mut messages = vec![call("1", "mcp__x__huge"), result("1", 100_000)];
    let dir = tempfile::tempdir().expect("临时目录");

    let first = trim_tool_results(&mut messages, Some(dir.path()), &budget(10_000, 5_000), "s");
    assert_eq!(first.trimmed, 1);
    let after_first = result_text(&messages, 1).to_string();

    let second = trim_tool_results(&mut messages, Some(dir.path()), &budget(10_000, 5_000), "s");
    assert_eq!(second.trimmed, 0, "已经是桩了就不该再动它");
    assert_eq!(result_text(&messages, 1), after_first);
}

#[test]
fn the_stub_points_at_the_spilled_file() {
    let mut messages = vec![call("1", "mcp__x__huge"), result("1", 100_000)];
    let home = tempfile::tempdir().expect("临时目录");
    let spill = home.path().join("spill");

    let report = trim_tool_results(&mut messages, Some(&spill), &budget(10_000, 5_000), "s");
    assert_eq!(report.trimmed, 1);

    let text = result_text(&messages, 1);
    assert!(text.contains("完整输出在 spill/"), "要指出回查路径：{text}");
    let relative = text
        .split("完整输出在 ")
        .nth(1)
        .and_then(|rest| rest.split('）').next())
        .expect("路径");
    let saved = std::fs::read_to_string(home.path().join(relative)).expect("读回落盘文件");
    assert_eq!(saved.chars().count(), 100_000, "落盘的必须是完整原文");
}
