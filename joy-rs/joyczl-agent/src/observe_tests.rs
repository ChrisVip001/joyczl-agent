//! Observation tests: the difference between "the tool said it wrote" and "the file is there".
//!
//! Both scenarios below are the **same tool call with the same output text** — only the
//! filesystem differs. That is the point: the ledger reads the world, not the model's prose.

use joyczl_provider::{ContentBlock, Message, Role};
use serde_json::json;

use crate::observe::{observe, Fact};

/// One assistant tool call + its result, as the loop would leave them in working memory.
fn round(tool: &str, args: serde_json::Value, output: &str) -> Vec<Message> {
    vec![
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "toolu_1".to_string(),
                name: tool.to_string(),
                input: args,
                extra: None,
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_1".to_string(),
                content: output.to_string(),
            }],
        },
    ]
}

#[tokio::test]
async fn a_real_write_becomes_an_artifact() {
    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::write(dir.path().join("report.md"), "结论：可以上线\n").expect("写文件");

    let messages = round(
        "write_file",
        json!({ "path": "report.md", "content": "结论：可以上线\n" }),
        "已写入 report.md（22 字节）",
    );
    let observation = observe(&messages, "turn1", 1, Some(dir.path()));

    assert_eq!(observation.steps.len(), 1, "一次工具调用就是一步");
    assert!(observation.steps[0].ok);
    assert_eq!(observation.steps[0].idempotency_key, "turn1:1");
    assert!(!observation.has_contradiction());
    let artifacts = observation.artifacts();
    assert_eq!(artifacts.len(), 1);
    assert!(artifacts[0].0.ends_with("report.md"));
    assert!(artifacts[0].1 > 0, "大小要实测");
}

#[tokio::test]
async fn a_write_that_left_no_file_is_a_contradiction() {
    let dir = tempfile::tempdir().expect("临时目录");

    // Same tool, same confident output — the file simply is not there.
    let messages = round(
        "write_file",
        json!({ "path": "report.md", "content": "结论：可以上线\n" }),
        "已写入 report.md（22 字节）",
    );
    let observation = observe(&messages, "turn1", 1, Some(dir.path()));

    assert!(observation.has_contradiction(), "说写了但没写，必须检出");
    assert!(observation.artifacts().is_empty(), "不存在的文件不是交付物");
    let said = observation.contradictions();
    assert_eq!(said.len(), 1);
    assert!(
        said[0].contains("report.md"),
        "要说清是哪个文件：{}",
        said[0]
    );
    assert!(
        matches!(&observation.facts[0], Fact::Contradiction { .. }),
        "事实必须是矛盾，不是写入"
    );
}

#[tokio::test]
async fn a_failed_write_with_no_file_is_not_a_contradiction() {
    let dir = tempfile::tempdir().expect("临时目录");
    let messages = round(
        "write_file",
        json!({ "path": "report.md", "content": "x" }),
        "Error: 这次写入没有被批准（或没人回答），已跳过。",
    );
    let observation = observe(&messages, "turn1", 1, Some(dir.path()));
    assert!(!observation.has_contradiction(), "失败且没写，是诚实的");
    assert!(!observation.steps[0].ok);
}

#[tokio::test]
async fn steps_keep_one_continuous_order_across_absorbed_passes() {
    let dir = tempfile::tempdir().expect("临时目录");
    let first = observe(
        &round("read_file", json!({ "path": "a.md" }), "内容"),
        "turn1",
        1,
        Some(dir.path()),
    );
    let second = observe(
        &round("read_file", json!({ "path": "b.md" }), "内容"),
        "turn1",
        1,
        Some(dir.path()),
    );

    let mut merged = first;
    merged.absorb(second);
    let seqs: Vec<i32> = merged.steps.iter().map(|step| step.seq).collect();
    assert_eq!(seqs, vec![1, 2], "第二轮要接在上一轮后面");
    assert_eq!(merged.steps[1].idempotency_key, "turn1:2");
}

#[tokio::test]
async fn a_non_write_tool_never_produces_facts() {
    let dir = tempfile::tempdir().expect("临时目录");
    let observation = observe(
        &round("run_command", json!({ "command": "ls" }), "a.md\nb.md"),
        "turn1",
        1,
        Some(dir.path()),
    );
    assert!(observation.facts.is_empty(), "只有写文件才有文件事实");
    assert_eq!(observation.steps.len(), 1);
}
