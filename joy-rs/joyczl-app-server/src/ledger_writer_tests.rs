//! Ledger writer tests: the record must be complete, spilled when big, and readable back.

use joyczl_provider::{ContentBlock, Message, Role};

use crate::ledger_writer::{entries_for_turn, payload_text, rebuild_turn, LedgerSink};

fn temp_home(name: &str) -> std::path::PathBuf {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().join(name);
    let _ = dir.keep();
    home
}

fn kinds(entries: &[joyczl_state::NewEntry]) -> Vec<&'static str> {
    entries.iter().map(|entry| entry.kind).collect()
}

/// 一轮的请求、工具调用、工具结果、回复，都成一类一行，顺序就是发生顺序。
#[test]
fn a_turn_becomes_ordered_rows() {
    let home = temp_home("ledger_entries");
    let sink = LedgerSink::new(&home, 32_768);
    let messages = vec![
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "看看这个报错".to_string(),
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "先读日志".to_string(),
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
                text: "日志不存在".to_string(),
            }],
        },
    ];

    let entries = entries_for_turn(&sink, "s1", "t1", "看看这个报错", &messages, "日志不存在");
    assert_eq!(
        kinds(&entries),
        vec!["user", "assistant", "tool_use", "tool_result", "assistant"],
        "请求只记一次（messages 里那份要跳过）"
    );
    assert!(
        entries[0].payload.contains("看看这个报错"),
        "小内容直接内联：{}",
        entries[0].payload
    );
    assert!(
        entries[2].payload.contains(r#""name":"read_file""#),
        "工具调用要带名字与参数：{}",
        entries[2].payload
    );
    assert!(
        entries[3].payload.contains("no such file"),
        "工具结果要带原文：{}",
        entries[3].payload
    );
}

/// 超阈值的内容转存成文件，行里留指针 + 摘要；指针能读回来，改一个字摘要就对不上。
#[test]
fn a_large_payload_spills_and_verifies() {
    let home = temp_home("ledger_spill");
    let sink = LedgerSink::new(&home, 64);
    let big = "日志行\n".repeat(200);
    let messages = vec![
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "看日志".to_string(),
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

    let entries = entries_for_turn(&sink, "s1", "t1", "看日志", &messages, "");
    let spilled = entries
        .iter()
        .find(|entry| entry.kind == "tool_result")
        .expect("有工具结果");
    assert!(
        spilled.payload.contains(r#""ptr":"ledger/s1/"#),
        "大内容该转存：{}",
        &spilled.payload[..60.min(spilled.payload.len())]
    );
    assert!(
        spilled.payload.len() < big.len() / 2,
        "行里只留指针，不该把正文再抄一遍（{} vs {}）",
        spilled.payload.len(),
        big.len()
    );
    assert!(home.join("ledger/s1").exists(), "文件真的落盘了");

    let text = payload_text(&home, &spilled.payload).expect("读回来");
    assert!(text.contains("日志行"), "内容完整");
    assert!(text.len() > big.len(), "读回来的是带 id 的完整 JSON");

    // 篡改：文件改一个字，读的时候必须报错，而不是把错的当对的。
    let dir = home.join("ledger/s1");
    let file = std::fs::read_dir(&dir)
        .expect("列目录")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .next()
        .expect("有文件");
    let mut text = std::fs::read_to_string(&file).expect("读文件");
    text.push_str("篡改");
    std::fs::write(&file, text).expect("改文件");
    assert!(
        payload_text(&home, &spilled.payload).is_err(),
        "被改过就必须报错"
    );
}

/// 重建：行读回来还原成消息块，同一角色的块合并在一条消息里。
#[test]
fn a_turn_rebuilds_into_messages() {
    let home = temp_home("ledger_rebuild");
    let sink = LedgerSink::new(&home, 32_768);
    let messages = vec![
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "看看这个报错".to_string(),
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "先读日志".to_string(),
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

    let entries = entries_for_turn(&sink, "s1", "t1", "看看这个报错", &messages, "");
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

    let rebuilt = rebuild_turn(&home, &rows).expect("重建");
    assert_eq!(rebuilt.len(), 3, "user / assistant / user(tool_result)");
    assert_eq!(rebuilt[1].role, Role::Assistant);
    assert_eq!(
        rebuilt[1].content.len(),
        2,
        "文本与工具调用合并在一条消息里"
    );
    assert_eq!(rebuilt[0].content.len(), 1, "请求只出现一次");
    match &rebuilt[2].content[0] {
        ContentBlock::ToolResult { content, .. } => assert_eq!(content, "no such file"),
        other => panic!("该是工具结果，实际 {other:?}"),
    }
}
