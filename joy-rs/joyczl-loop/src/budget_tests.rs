//! Intra-turn budget rules: big ones only, oldest first (newest kept), idempotent,
//! self-budgeted untouched.

use joyczl_provider::{ContentBlock, Message, Role};

use super::budget::{should_wrap_up, trim_tool_results, ToolResultBudget};

/// Both ways of switching it off have to really switch it off: `None` (unset) and 0 (set to
/// zero) mean the same thing.
#[test]
fn no_budget_never_wraps_up() {
    assert!(!should_wrap_up(None, 1_000_000, 1_000_000));
    assert!(!should_wrap_up(Some(0), 1_000_000, 1_000_000));
}

/// The threshold is 80%, and "what the next round will probably cost" is part of it — without
/// that, "80%" does not stop until 95%.
#[test]
fn the_horizon_is_eighty_percent_of_the_budget() {
    assert!(
        !should_wrap_up(Some(1000), 700, 99),
        "at 79.9% it carries on"
    );
    assert!(should_wrap_up(Some(1000), 700, 100), "800 = 80%, wrap up");
    assert!(should_wrap_up(Some(1000), 900, 0), "already over the line");
    assert!(
        should_wrap_up(Some(1000), 0, 800),
        "nothing spent yet, but the next round needs 800: the forecast counts too"
    );
}

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
        // Old tests keep the historical "no free pass" shape; `keep_recent` has its own test.
        keep_recent: 0,
    }
}

fn result_text(messages: &[Message], index: usize) -> &str {
    match &messages[index].content[0] {
        ContentBlock::ToolResult { content, .. } => content,
        _ => panic!("not a tool result"),
    }
}

/// Trimming walks oldest-first and never touches the newest results: those are the ones
/// the model is reasoning about right now.
#[test]
fn the_newest_results_survive_and_the_oldest_go_first() {
    let mut messages = vec![
        call("1", "mcp__x__a"),
        result("1", 400),
        call("2", "mcp__x__b"),
        result("2", 400),
        call("3", "mcp__x__c"),
        result("3", 400),
    ];
    let budget = ToolResultBudget {
        total_chars: 500,
        per_result_chars: 100,
        keep_recent: 2,
    };

    let report = trim_tool_results(&mut messages, None, &budget, "s");

    assert_eq!(report.trimmed, 1, "only the oldest one should be swapped");
    assert!(
        result_text(&messages, 1).chars().count() < 400,
        "the oldest one should become a stub: {}",
        result_text(&messages, 1)
    );
    assert_eq!(
        result_text(&messages, 3).chars().count(),
        400,
        "the next oldest stays as it is"
    );
    assert_eq!(
        result_text(&messages, 5).chars().count(),
        400,
        "the newest stays as it is"
    );
    assert!(
        report.still_over_budget,
        "when keeping the newest still leaves it over budget, it has to be said plainly"
    );
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
    assert!(!result_text(&messages, 1).contains("truncated"));
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

    assert_eq!(
        report.trimmed, 2,
        "both have to become stubs for it to fit: {report:?}"
    );
    assert!(!report.still_over_budget);
    assert!(
        result_text(&messages, 1).contains("truncated"),
        "the first one should be swapped"
    );
    assert!(
        result_text(&messages, 3).contains("truncated"),
        "the second one should be swapped"
    );
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

    assert_eq!(
        report.trimmed, 1,
        "swapping the biggest one is enough for 60k: {report:?}"
    );
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

    assert_eq!(
        report.trimmed, 0,
        "one file per small result, a hundred of them, would cost more"
    );
    assert!(
        report.still_over_budget,
        "it has to admit it fell short rather than pretend it dealt with it"
    );
    assert!(!result_text(&messages, 1).contains("truncated"));
}

#[test]
fn a_self_budgeted_tool_is_left_to_the_tool_itself() {
    // run_command spills and truncates to 8000 itself; another wrapper just swaps paths around.
    let mut messages = vec![call("1", "run_command"), result("1", 40_000)];
    let report = trim_tool_results(&mut messages, None, &budget(10_000, 5_000), "s");
    assert_eq!(report.trimmed, 0);
    assert!(!result_text(&messages, 1).contains("truncated"));
}

#[test]
fn trimming_is_idempotent() {
    let mut messages = vec![call("1", "mcp__x__huge"), result("1", 100_000)];
    let dir = tempfile::tempdir().expect("temp dir");

    let first = trim_tool_results(&mut messages, Some(dir.path()), &budget(10_000, 5_000), "s");
    assert_eq!(first.trimmed, 1);
    let after_first = result_text(&messages, 1).to_string();

    let second = trim_tool_results(&mut messages, Some(dir.path()), &budget(10_000, 5_000), "s");
    assert_eq!(
        second.trimmed, 0,
        "already a stub, so it must not be touched again"
    );
    assert_eq!(result_text(&messages, 1), after_first);
}

#[test]
fn the_stub_points_at_the_spilled_file() {
    let mut messages = vec![call("1", "mcp__x__huge"), result("1", 100_000)];
    let home = tempfile::tempdir().expect("temp dir");
    let spill = home.path().join("spill");

    let report = trim_tool_results(&mut messages, Some(&spill), &budget(10_000, 5_000), "s");
    assert_eq!(report.trimmed, 1);

    let text = result_text(&messages, 1);
    assert!(
        text.contains("full output at spill/"),
        "it has to point at the way back: {text}"
    );
    let relative = text
        .split("full output at ")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .expect("the path");
    let saved =
        std::fs::read_to_string(home.path().join(relative)).expect("read the spilled file back");
    assert_eq!(
        saved.chars().count(),
        100_000,
        "what reaches the disk has to be the complete original text"
    );
}
