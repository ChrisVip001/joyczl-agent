//! trace / usage 落盘 —— LLM-Ops 的账本。
//!
//! 两本账：
//!
//!   * `<home>/traces/<日期>.jsonl`  每轮一行：用户说了什么、怎么答的、
//!     门和图怎么走的、花了多少 token。**始终开启** —— 没有配置开关，
//!     观测不是可选项。
//!   * `<home>/usage.jsonl`          永续账本：每轮一行 provider / model /
//!     token 数。追加永不改写。
//!
//! 两条都是 append-only 单行写入。写失败只往 stderr 喊一声 ——
//! 账本坏了不该连累对话，但也不能悄悄闭嘴。

use std::io::Write;
use std::path::Path;

use chrono::Local;

/// 追加一行 JSON。目录不存在就建；写失败往 stderr 报，不 panic。
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

/// 一轮的完整 trace。`meta` 是协议里的 TurnMeta（门/图/工具/模型都在里面），
/// 外面再包上本轮的输入输出 —— 排查「它当时为什么这么答」靠这一行就够。
pub fn record_turn(home: &Path, record: &serde_json::Value) {
    let date = Local::now().format("%Y-%m-%d");
    append_jsonl(
        &home.join("traces").join(format!("{date}.jsonl")),
        &record.to_string(),
    );
}

/// 一轮的 token 用量入账。账本按轮记：provider / model / tokens / 迭代数。
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
        // home 指向一个文件：目录建不出来，但调用方照常往下走。
        let dir = tempfile::tempdir().expect("临时目录");
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "not a directory").unwrap();
        record_turn(&blocker, &serde_json::json!({"turnId": "t"}));
        record_usage(&blocker, &serde_json::json!({"inputTokens": 1}));
    }
}
