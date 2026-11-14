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
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("report.md"), "verdict: ready to ship\n")
        .expect("write the file");

    let messages = round(
        "write_file",
        json!({ "path": "report.md", "content": "verdict: ready to ship\n" }),
        "wrote report.md (22 bytes)",
    );
    let observation = observe(&messages, "turn1", 1, Some(dir.path()));

    assert_eq!(observation.steps.len(), 1, "one tool call is one step");
    assert!(observation.steps[0].ok);
    assert_eq!(observation.steps[0].idempotency_key, "turn1:1");
    assert!(!observation.has_contradiction());
    let artifacts = observation.artifacts();
    assert_eq!(artifacts.len(), 1);
    assert!(artifacts[0].0.ends_with("report.md"));
    assert!(
        artifacts[0].1 > 0,
        "the size has to be measured, not assumed"
    );
}

#[tokio::test]
async fn a_write_that_left_no_file_is_a_contradiction() {
    let dir = tempfile::tempdir().expect("temp dir");

    // Same tool, same confident output — the file simply is not there.
    let messages = round(
        "write_file",
        json!({ "path": "report.md", "content": "verdict: ready to ship\n" }),
        "wrote report.md (22 bytes)",
    );
    let observation = observe(&messages, "turn1", 1, Some(dir.path()));

    assert!(
        observation.has_contradiction(),
        "claiming a write that never happened has to be caught"
    );
    assert!(
        observation.artifacts().is_empty(),
        "a file that does not exist is not a deliverable"
    );
    let said = observation.contradictions();
    assert_eq!(said.len(), 1);
    assert!(
        said[0].contains("report.md"),
        "it has to say which file: {}",
        said[0]
    );
    assert!(
        matches!(&observation.facts[0], Fact::Contradiction { .. }),
        "the fact has to be a contradiction, not a write"
    );
}

#[tokio::test]
async fn a_failed_write_with_no_file_is_not_a_contradiction() {
    let dir = tempfile::tempdir().expect("temp dir");
    let messages = round(
        "write_file",
        json!({ "path": "report.md", "content": "x" }),
        "Error: this write was not approved (or nobody answered), so it was skipped.",
    );
    let observation = observe(&messages, "turn1", 1, Some(dir.path()));
    assert!(
        !observation.has_contradiction(),
        "failing and not writing is honest"
    );
    assert!(!observation.steps[0].ok);
}

#[tokio::test]
async fn steps_keep_one_continuous_order_across_absorbed_passes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let first = observe(
        &round("read_file", json!({ "path": "a.md" }), "content"),
        "turn1",
        1,
        Some(dir.path()),
    );
    let second = observe(
        &round("read_file", json!({ "path": "b.md" }), "content"),
        "turn1",
        1,
        Some(dir.path()),
    );

    let mut merged = first;
    merged.absorb(second);
    let seqs: Vec<i32> = merged.steps.iter().map(|step| step.seq).collect();
    assert_eq!(seqs, vec![1, 2], "the second round follows the first");
    assert_eq!(merged.steps[1].idempotency_key, "turn1:2");
}

#[tokio::test]
async fn a_non_write_tool_never_produces_facts() {
    let dir = tempfile::tempdir().expect("temp dir");
    let observation = observe(
        &round("run_command", json!({ "command": "ls" }), "a.md\nb.md"),
        "turn1",
        1,
        Some(dir.path()),
    );
    assert!(
        observation.facts.is_empty(),
        "only a file write produces a file fact"
    );
    assert_eq!(observation.steps.len(), 1);
}

#[test]
fn human_receipts_do_not_count_as_observed_successful_tool_calls() {
    let observation = observe(
        &round(
            "external_write",
            json!({"value":42}),
            "Human reconciliation: external receipt tx-42 confirmed",
        ),
        "turn",
        1,
        None,
    );
    assert_eq!(observation.steps.len(), 1);
    assert!(!observation.steps[0].ok);
}
