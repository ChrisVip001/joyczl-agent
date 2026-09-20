//! 评测报告：最新一份 verdict + 永续的运行历史。
//!
//! `eval_report.json` 给人和 CI 看「最近跑成什么样」；`eval_runs.jsonl`
//! 是 append-only 的历史 —— 模型换了、prompt 改了，分数曲线就长在这里。

use std::path::Path;

use anyhow::Result;
use chrono::Local;

/// 写最新报告并追加一条历史。写失败只警告 —— 账本不该拦住评测本身。
pub fn write(home: &Path, suite: &str, payload: serde_json::Value) -> Result<()> {
    let record = serde_json::json!({
        "suite": suite,
        "result": payload,
        "ran_at": Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    });
    std::fs::create_dir_all(home)?;
    let report_path = home.join("eval_report.json");
    if let Err(e) = std::fs::write(&report_path, record.to_string()) {
        eprintln!(
            "(joy) eval_report.json 写不进去（{}）：{e}",
            report_path.display()
        );
    }
    let history = home.join("eval_runs.jsonl");
    if let Some(parent) = history.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&history)
    {
        Ok(mut file) => {
            use std::io::Write;
            if let Err(e) = writeln!(file, "{record}") {
                eprintln!("(joy) eval_runs.jsonl 追加失败：{e}");
            }
        }
        Err(e) => eprintln!("(joy) eval_runs.jsonl 打不开：{e}"),
    }
    Ok(())
}
