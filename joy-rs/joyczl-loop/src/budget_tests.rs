//! 轮内预算的四条纪律：够大才动、从大到小、幂等、自预算的不碰。

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

    // 预算 1 万：换掉最大的那条还超，于是接着换第二条 —— 直到装得下。
    let report = trim_tool_results(&mut messages, None, &budget(10_000, 30_000), "s");

    assert_eq!(report.trimmed, 2, "两条都得换成桩才装得下：{report:?}");
    assert!(!report.still_over_budget);
    assert!(result_text(&messages, 1).contains("已截断"), "第一条该换掉");
    assert!(result_text(&messages, 3).contains("已截断"), "第二条该换掉");
    // 小的那条原样保留 —— 给一千字符建一个文件不值（它也没到门槛）。
    assert_eq!(result_text(&messages, 5), "x".repeat(1_000));
    assert!(report.total_chars <= 10_000, "{report:?}");
}

#[test]
fn it_stops_as_soon_as_the_batch_fits() {
    // 与 deepseek-harness 的循环同一条：`if total <= max: break` —— 一旦装得下
    // 就停手，不会顺手把「本来还能留着的」也换掉。
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
    // 100 条各 1000 字符 = 10 万，超预算 5 万；但每一条都没到 3 万的门槛。
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
    // run_command 自己会落盘并截到 8000；再套一层只会把路径换来换去。
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
