//! The three invariants of `spill.rs`: the stub fits, whole lines only, spilling failure never lies.

use super::spill::{spill_text, stub};

fn long_text(lines: usize, width: usize) -> String {
    (0..lines)
        .map(|i| format!("line-{i}-{}", "x".repeat(width)))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_text_that_already_fits_is_left_alone() {
    let text = long_text(3, 10);
    assert_eq!(stub(None, &text, 10_000, "t"), None);
    // Even a budget of 0 must not produce a pointless stub (a 0 budget is a config error, not a content problem).
    assert_eq!(stub(None, &text, 0, "t"), None);
}

#[test]
fn the_stub_fits_the_budget_and_only_cuts_complete_lines() {
    let dir = tempfile::tempdir().expect("临时目录");
    let text = long_text(200, 40);
    let budget = 400;

    let stub = stub(Some(dir.path()), &text, budget, "tool-result").expect("该做桩");
    assert!(
        stub.text.chars().count() <= budget,
        "桩必须放得下：{} > {budget}",
        stub.text.chars().count()
    );
    assert!(stub.path.is_some(), "落了盘就该带上路径");
    assert!(stub.omitted_lines > 0);

    // Except the omission-notice line, every line must be a line that **really exists** in the original:
    // half a line (half a JSON, half a log entry) reads worse than one fewer line.
    for line in stub.text.lines() {
        if line.starts_with('…') {
            assert!(line.contains("truncated"), "省略说明要诚实：{line}");
            assert!(line.contains("full output at"), "有路径就要写出来：{line}");
            continue;
        }
        assert!(
            text.lines().any(|original| original == line),
            "这一行不是原文的完整行：{line}"
        );
    }
}

#[test]
fn without_a_directory_it_still_stubs_but_never_promises_a_path() {
    let text = long_text(200, 40);
    let stub = stub(None, &text, 300, "tool-result").expect("该做桩");
    assert_eq!(stub.path, None);
    assert!(
        !stub.text.contains("full output at"),
        "没落盘就不能说有文件：{}",
        stub.text
    );
    assert!(stub.text.contains("truncated"), "{}", stub.text);
    assert!(stub.text.chars().count() <= 300);
}

#[test]
fn one_line_longer_than_the_budget_gets_no_fake_head() {
    let dir = tempfile::tempdir().expect("临时目录");
    // One line of 5000 chars, budget 200: not even one line fits.
    let text = "y".repeat(5_000);
    let stub = stub(Some(dir.path()), &text, 200, "tool-result").expect("该做桩");
    assert!(
        stub.text.chars().count() <= 200,
        "桩必须放得下：{}",
        stub.text.chars().count()
    );
    // "No half lines" is verified by **lines**: the whole text may contain other characters
    // (a path would contain y), but no line is a fragment of that oversized original.
    assert!(
        stub.text.lines().all(|line| line.chars().count() < 200),
        "放不下一整行就别放半行：{:?}",
        stub.text
    );
    assert!(stub.text.contains("full output at"), "{}", stub.text);
}

#[test]
fn spill_writes_the_whole_text_and_reports_a_relative_path() {
    let home = tempfile::tempdir().expect("临时目录");
    let spill_dir = home.path().join("spill");
    let text = long_text(500, 40);

    let path = spill_text(&spill_dir, &text, "command").expect("落盘");
    assert!(path.starts_with("spill/"), "路径要相对 home：{path}");
    let saved = std::fs::read_to_string(home.path().join(&path)).expect("读回来");
    assert_eq!(saved, text, "落盘的必须是完整原文");
}

#[test]
fn the_stub_is_smaller_than_what_it_replaces() {
    // Regression guard: no stub when the budget exceeds the original; a produced stub is always shorter than the original.
    let text = long_text(100, 40);
    let stub = stub(None, &text, 500, "t").expect("该做桩");
    assert!(
        stub.text.chars().count() < text.chars().count(),
        "桩不该比原文更长"
    );
}

#[test]
fn a_small_preview_keeps_the_recovery_path_whole() {
    let dir = tempfile::tempdir().unwrap();
    let result = stub(Some(dir.path()), &"x".repeat(500), 96, "tool-result").unwrap();
    let path = result.path.unwrap();
    assert!(result.text.contains(&path), "{}", result.text);
    assert!(result.text.chars().count() <= 96);
    let tiny = stub(Some(dir.path()), &"x".repeat(500), 20, "tool-result").unwrap();
    assert!(!tiny.text.contains("full output at"));
    assert!(tiny.text.chars().count() <= 20);
}
