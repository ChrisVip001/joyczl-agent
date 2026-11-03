//! Eval reports: the latest verdict + a permanent run history.
//!
//! `eval_report.json` shows people and CI "how the latest run went"; `eval_runs.jsonl`
//! is the append-only history — swap models, change prompts, and the score curve grows here.

use std::path::Path;

use anyhow::Result;
use chrono::Local;

/// Writes the latest report and appends one history entry. Write failures only warn — the ledger must not block the eval itself.
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
