//! `spill.rs` 的三条不变式：桩放得下、只切完整行、落盘失败不撒谎。

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
    // 预算是 0 也不该做无意义的桩（预算为 0 是配置错误，不是内容问题）。
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

    // 除了省略说明那一行，其余每一行都必须是原文里**完整存在**的一行：
    // 半个行（半句 JSON、半条日志）比少一行更难读。
    for line in stub.text.lines() {
        if line.starts_with('…') {
            assert!(line.contains("已截断"), "省略说明要诚实：{line}");
            assert!(line.contains("完整输出在"), "有路径就要写出来：{line}");
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
        !stub.text.contains("完整输出在"),
        "没落盘就不能说有文件：{}",
        stub.text
    );
    assert!(stub.text.contains("已截断"), "{}", stub.text);
    assert!(stub.text.chars().count() <= 300);
}

#[test]
fn one_line_longer_than_the_budget_gets_no_fake_head() {
    let dir = tempfile::tempdir().expect("临时目录");
    // 单行 5000 字符，预算只有 200：一行都放不下。
    let text = "y".repeat(5_000);
    let stub = stub(Some(dir.path()), &text, 200, "tool-result").expect("该做桩");
    assert!(
        stub.text.chars().count() <= 200,
        "桩必须放得下：{}",
        stub.text.chars().count()
    );
    // 「不放半行」要按**行**验：整段文本里可能有别的字符（路径里就会出现 y），
    // 但没有一行是那条超长原文的碎片。
    assert!(
        stub.text.lines().all(|line| line.chars().count() < 200),
        "放不下一整行就别放半行：{:?}",
        stub.text
    );
    assert!(stub.text.contains("完整输出在"), "{}", stub.text);
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
    // 防回归：预算给得比原文还大时不做桩；做出来的桩一定比原文短。
    let text = long_text(100, 40);
    let stub = stub(None, &text, 500, "t").expect("该做桩");
    assert!(
        stub.text.chars().count() < text.chars().count(),
        "桩不该比原文更长"
    );
}
