//! Ledger writer tests: the record must be complete, spilled when big, and readable back.

use joyczl_provider::{ContentBlock, Message, Role};

use crate::ledger_writer::{entries_for_turn, payload_text, rebuild_turn, LedgerSink};

fn temp_home(name: &str) -> std::path::PathBuf {
    let dir = tempfile::tempdir().expect("temporary directory");
    let home = dir.path().join(name);
    let _ = dir.keep();
    home
}

fn kinds(entries: &[joyczl_state::NewEntry]) -> Vec<&'static str> {
    entries.iter().map(|entry| entry.kind).collect()
}

/// A turn's request, tool calls, tool results and reply each become one row, in the order they
/// happened.
#[test]
fn a_turn_becomes_ordered_rows() {
    let home = temp_home("ledger_entries");
    let sink = LedgerSink::new(&home, 32_768);
    let messages = vec![
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "take a look at this error".to_string(),
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "read the log first".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "call-1".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({"path": "app.log"}),
                    extra: None,
                },
            ],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call-1".to_string(),
                content: "no such file".to_string(),
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "the log does not exist".to_string(),
            }],
        },
    ];

    let entries = entries_for_turn(
        &sink,
        "s1",
        "t1",
        "take a look at this error",
        &messages,
        "the log does not exist",
    );
    assert_eq!(
        kinds(&entries),
        vec!["user", "assistant", "tool_use", "tool_result", "assistant"],
        "the request is recorded once (the copy inside messages is skipped)"
    );
    assert!(
        entries[0].payload.contains("take a look at this error"),
        "small content is inlined directly: {}",
        entries[0].payload
    );
    assert!(
        entries[2].payload.contains(r#""name":"read_file""#),
        "a tool call has to carry its name and arguments: {}",
        entries[2].payload
    );
    assert!(
        entries[3].payload.contains("no such file"),
        "a tool result has to carry its text: {}",
        entries[3].payload
    );
}

/// Content over the threshold is spilled to a file and the row keeps a pointer plus a digest;
/// the pointer reads back, and one changed character breaks the digest.
#[test]
fn a_large_payload_spills_and_verifies() {
    let home = temp_home("ledger_spill");
    let sink = LedgerSink::new(&home, 64);
    let big = "log line\n".repeat(200);
    let messages = vec![
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "look at the log".to_string(),
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call-9".to_string(),
                content: big.clone(),
            }],
        },
    ];

    let entries = entries_for_turn(&sink, "s1", "t1", "look at the log", &messages, "");
    let spilled = entries
        .iter()
        .find(|entry| entry.kind == "tool_result")
        .expect("there is a tool result");
    assert!(
        spilled.payload.contains(r#""ptr":"ledger/s1/"#),
        "big content has to be spilled: {}",
        &spilled.payload[..60.min(spilled.payload.len())]
    );
    assert!(
        spilled.payload.len() < big.len() / 2,
        "the row keeps only the pointer, not a second copy of the body ({} vs {})",
        spilled.payload.len(),
        big.len()
    );
    assert!(
        home.join("ledger/s1").exists(),
        "the file really reached the disk"
    );

    let text = payload_text(&home, &spilled.payload).expect("read it back");
    assert!(text.contains("log line"), "the content is complete");
    assert!(
        text.len() > big.len(),
        "what reads back is the whole JSON with ids"
    );

    // Tampering: change one character in the file and reading has to fail, rather than quietly
    // treating the wrong bytes as right.
    let dir = home.join("ledger/s1");
    let file = std::fs::read_dir(&dir)
        .expect("list the directory")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .next()
        .expect("there is a file");
    let mut text = std::fs::read_to_string(&file).expect("read the file");
    text.push_str("tampered");
    std::fs::write(&file, text).expect("change the file");
    assert!(
        payload_text(&home, &spilled.payload).is_err(),
        "once altered it has to report an error"
    );
}

/// Rebuild: the rows come back as message blocks, and blocks of the same role merge into one
/// message.
#[test]
fn a_turn_rebuilds_into_messages() {
    let home = temp_home("ledger_rebuild");
    let sink = LedgerSink::new(&home, 32_768);
    let messages = vec![
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "take a look at this error".to_string(),
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "read the log first".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "call-1".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({"path": "app.log"}),
                    extra: None,
                },
            ],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call-1".to_string(),
                content: "no such file".to_string(),
            }],
        },
    ];

    let entries = entries_for_turn(
        &sink,
        "s1",
        "t1",
        "take a look at this error",
        &messages,
        "",
    );
    let rows: Vec<joyczl_state::EntryRow> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| joyczl_state::EntryRow {
            seq: index as i64,
            kind: entry.kind.to_string(),
            payload: entry.payload.clone(),
            digest: joyczl_state::ledger::digest_of(entry.payload.as_bytes()),
            fidelity: entry.fidelity.to_string(),
            at: String::new(),
        })
        .collect();

    let rebuilt = rebuild_turn(&home, &rows).expect("rebuild");
    assert_eq!(rebuilt.len(), 3, "user / assistant / user(tool_result)");
    assert_eq!(rebuilt[1].role, Role::Assistant);
    assert_eq!(
        rebuilt[1].content.len(),
        2,
        "text and a tool call inside one message"
    );
    assert_eq!(rebuilt[0].content.len(), 1, "the request appears once only");
    match &rebuilt[2].content[0] {
        ContentBlock::ToolResult { content, .. } => assert_eq!(content, "no such file"),
        other => panic!("this should be a tool result, actually {other:?}"),
    }
}
