//! trace / usage persistence — the LLM-Ops ledgers.
//!
//! Two ledgers:
//!
//!   * `<home>/traces/<date>.jsonl`  one line per turn: what the user said, how it was answered,
//!     how the gate and graph went, how many tokens were spent. **Always on** — there is no config
//!     switch; observability is not optional.
//!   * `<home>/usage.jsonl`          the perpetual ledger: one line per turn of provider / model /
//!     token counts. Append-only, never rewritten.
//!
//! Both are append-only single-line writes. A write failure only shouts to stderr —
//! a broken ledger must not drag the conversation down, but it must not go silent either.

use std::io::Write;
use std::path::Path;

use chrono::Local;

/// Append one JSON line. Create the directory if missing; report write failures to stderr, never panic.
fn append_jsonl(path: &Path, line: &str) {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("(joy) 落盘目录建不了（{}）：{e}", parent.display());
            return;
        }
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(mut file) => {
            if let Err(e) = file.write_all(format!("{line}\n").as_bytes()) {
                eprintln!("(joy) 写账本失败（{}）：{e}", path.display());
            }
        }
        Err(e) => eprintln!("(joy) 打不开账本（{}）：{e}", path.display()),
    }
}

/// The full trace of one turn. `meta` is the protocol's TurnMeta (gate/graph/tools/model all inside),
/// wrapped with this turn's input and output — that one line is enough to trace "why it answered that way then".
pub fn record_turn(home: &Path, record: &serde_json::Value) {
    let date = Local::now().format("%Y-%m-%d");
    append_jsonl(
        &home.join("traces").join(format!("{date}.jsonl")),
        &record.to_string(),
    );
}

/// Book one turn's token usage. The ledger records per turn: provider / model / tokens / iterations.
pub fn record_usage(home: &Path, record: &serde_json::Value) {
    append_jsonl(&home.join("usage.jsonl"), &record.to_string());
}

#[cfg(test)]
mod trace_tests {
    use super::*;

    #[test]
    fn lines_land_as_jsonl_and_survive_failures() {
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path();

        record_turn(home, &serde_json::json!({"turnId": "t1"}));
        record_turn(home, &serde_json::json!({"turnId": "t2"}));
        record_usage(home, &serde_json::json!({"inputTokens": 10}));

        let date = Local::now().format("%Y-%m-%d").to_string();
        let trace = std::fs::read_to_string(home.join("traces").join(format!("{date}.jsonl")))
            .expect("trace 文件");
        let lines: Vec<&str> = trace.lines().collect();
        assert_eq!(lines.len(), 2, "两轮两行：{trace}");
        for line in &lines {
            let value: serde_json::Value = serde_json::from_str(line).expect("每行都是合法 JSON");
            assert!(value.get("turnId").is_some());
        }

        let usage = std::fs::read_to_string(home.join("usage.jsonl")).expect("usage 文件");
        assert_eq!(usage.lines().count(), 1, "{usage}");
    }

    #[test]
    fn a_bad_home_warns_instead_of_panicking() {
        // home points at a file: the directory cannot be created, but the caller proceeds as usual.
        let dir = tempfile::tempdir().expect("临时目录");
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "not a directory").unwrap();
        record_turn(&blocker, &serde_json::json!({"turnId": "t"}));
        record_usage(&blocker, &serde_json::json!({"inputTokens": 1}));
    }
}
