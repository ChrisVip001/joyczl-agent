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
            "(joy) could not write eval_report.json ({}): {e}",
            report_path.display()
        );
    }
    // Per-suite copy: the long benchmark and the release gate answer different questions, and a
    // shared "latest" file would let one run quietly overwrite the other's verdict.
    let suite_path = home.join(format!("eval_report_{suite}.json"));
    if let Err(e) = std::fs::write(&suite_path, record.to_string()) {
        eprintln!("(joy) could not write {}: {e}", suite_path.display());
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
                eprintln!("(joy) could not append to eval_runs.jsonl: {e}");
            }
        }
        Err(e) => eprintln!("(joy) could not open eval_runs.jsonl: {e}"),
    }
    Ok(())
}
