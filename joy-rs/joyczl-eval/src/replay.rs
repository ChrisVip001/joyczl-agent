//! Replay cases: a **recorded conversation** (what the human said + what the model answered) plus
//! the event sequence it must produce.
//!
//! Why a separate file shape at all, when deterministic cases are already "scripted replies +
//! assertions"? Because a replay answers a different question. A deterministic case asks "is this
//! behavior still correct"; a replay asks "**does this run still produce the same sequence of
//! events**" — approvals, streaming chunks, reconnects, compaction. Those only exist as a timeline,
//! so a replay file must state one.
//!
//! What this module deliberately does **not** do: run anything. A case is converted into the
//! existing [`Scenario`] and then goes through `run_scenario` + `score` exactly like a deterministic
//! case. Two runners would drift; one runner with two loaders cannot.
//!
//! File format (one JSON object per line, `#` comments, `*.jsonl` — same reader rules as evals):

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::scenario::{
    ApprovalScript, Expectations, Scenario, ScenarioTask, ScriptedResponse, SettingsOverrides,
    TurnScript,
};

/// The recording shape this build understands. Bump it when an old file would be read **wrong**
/// (not when a new optional field appears — that is what `#[serde(default)]` is for).
pub const SCHEMA_VERSION: u32 = 1;

/// One recorded conversation. `recorded.turns` carries the dialogue; `expect.events` is mandatory.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayCase {
    pub id: String,
    #[serde(default)]
    pub description: String,
    /// Missing = 0 = rejected on purpose: an unversioned recording cannot be trusted to mean
    /// what the reader thinks it means.
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub settings: SettingsOverrides,
    #[serde(default)]
    pub files: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub prereq: Option<String>,
    #[serde(default)]
    pub task: Option<ScenarioTask>,
    pub recorded: Recorded,
    #[serde(default)]
    pub expect: Expectations,
}

/// The "tape": the turns that were recorded.
#[derive(Debug, Clone, Deserialize)]
pub struct Recorded {
    pub turns: Vec<RecordedTurn>,
}

/// One recorded turn: the human's words, the model's scripted replies, and how a human answered
/// any approval question (or did not — `silence` is a scripted answer too).
#[derive(Debug, Clone, Deserialize)]
pub struct RecordedTurn {
    /// What the human said.
    pub say: String,
    /// The retrieval gate's script for this turn. Unset = the gate gets non-JSON (fail-open).
    #[serde(default)]
    pub gate: Option<serde_json::Value>,
    /// The model's replies in **call order**, gate call excluded (the gate script above is first).
    #[serde(default)]
    pub replies: Vec<ScriptedResponse>,
    #[serde(default)]
    pub approval: Option<ApprovalScript>,
    /// The workspace this turn picked (`{{home}}` expands to the case's temp home).
    #[serde(default)]
    pub cwd: Option<String>,
}

impl ReplayCase {
    /// The one job of this type: become a `Scenario`. No hidden state is carried over, so a
    /// difference in behavior between a replay and a deterministic case is never "the loader".
    pub fn into_scenario(self) -> Scenario {
        let turns = self
            .recorded
            .turns
            .into_iter()
            .map(|turn| TurnScript {
                message: turn.say,
                session_id: None,
                gate: turn.gate,
                responses: turn.replies,
                apply_patch: None,
                interrupt_after_deltas: None,
                cwd: turn.cwd,
                restart: false,
                approval: turn.approval,
            })
            .collect();
        Scenario {
            id: self.id,
            description: self.description,
            // Multi-turn from the start: a recording is always a sequence.
            message: None,
            gate: None,
            responses: Vec::new(),
            interrupt_after_deltas: None,
            cwd: None,
            turns: Some(turns),
            expect: self.expect,
            settings: self.settings,
            files: self.files,
            prereq: self.prereq,
            task: self.task,
            recovery: false,
            plan: None,
        }
    }
}

/// Reads recordings: each path is a `.jsonl` file or a directory (its `*.jsonl` files are read).
///
/// Every line is validated on load — an unversioned file or an empty `events` list is a **loader
/// error**, not a silently-passing case. "A replay that asserts nothing" would be a green light
/// with nothing behind it.
pub fn load_all(paths: &[PathBuf]) -> Result<Vec<ReplayCase>> {
    let mut cases = Vec::new();
    for path in paths {
        let files: Vec<PathBuf> = if path.is_dir() {
            std::fs::read_dir(path)
                .with_context(|| format!("读不了回放目录：{}", path.display()))?
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
                .collect()
        } else {
            vec![path.clone()]
        };
        // Sorted: cases run in a stable order, so the printed table does not shuffle per machine.
        let mut files = files;
        files.sort();

        for file in files {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("读不了回放文件：{}", file.display()))?;
            for (line_number, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let case: ReplayCase = serde_json::from_str(line).map_err(|e| {
                    anyhow::anyhow!(
                        "{} 第 {} 行不是合法回放用例：{e}",
                        file.display(),
                        line_number + 1
                    )
                })?;
                let where_ = format!(
                    "{} 第 {} 行（{}）",
                    file.display(),
                    line_number + 1,
                    case.id
                );
                if case.schema_version != SCHEMA_VERSION {
                    anyhow::bail!(
                        "{where_}：schemaVersion={} 不认识（这份构建只读 {SCHEMA_VERSION}）。\
                         老的录制文件不能猜着读 —— 换一份，或按 evals/replay/README.md 重录。",
                        case.schema_version
                    );
                }
                if case.recorded.turns.is_empty() {
                    anyhow::bail!("{where_}：recorded.turns 是空的，没有任何对话可回放");
                }
                if case.expect.events.is_empty() {
                    anyhow::bail!(
                        "{where_}：expect.events 是空的 —— 回放用例必须声明它期望的事件序列，\
                         否则它只是在重复一条确定性用例"
                    );
                }
                cases.push(case);
            }
        }
    }
    Ok(cases)
}

#[cfg(test)]
mod replay_tests {
    use super::*;

    // One line on purpose: the reader is a JSONL reader, and a recording file is one line per case.
    const ONE: &str = concat!(
        r#"{"id":"demo","description":"一轮：批准后写文件","schemaVersion":1,"#,
        r#""settings":{"write_enabled":true,"approval":"on-request"},"#,
        r#""recorded":{"turns":[{"say":"写到 {{home}}/a.md","#,
        r#""gate":{"retrieve":false,"query":"","reason":"note"},"#,
        r#""replies":[{"tool_use":{"name":"write_file","args":{"path":"{{home}}/a.md","content":"x"}}},"#,
        r#"{"text":"写好了。"}],"approval":{"answer":"yes","remember":"session"}}]},"#,
        r#""expect":{"events":["turnStarted","approvalRequested","turnCompleted"]}}"#
    );

    fn tmp(name: &str, body: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join(name);
        std::fs::write(&path, body).expect("写文件");
        (dir, path)
    }

    #[test]
    fn a_recording_becomes_a_scenario_without_losing_anything() {
        let (_dir, path) = tmp("one.jsonl", &format!("# 注释行\n{ONE}\n"));
        let mut cases = load_all(&[path]).expect("加载");
        assert_eq!(cases.len(), 1, "注释行不该算用例");

        let case = cases.remove(0);
        assert_eq!(case.recorded.turns.len(), 1);
        let scenario = case.into_scenario();
        let turns = scenario.effective_turns();
        assert_eq!(turns.len(), 1, "多轮要按轮展开");
        assert_eq!(turns[0].message, "写到 {{home}}/a.md");
        assert_eq!(turns[0].responses.len(), 2, "脚本回复一条都不能丢");
        assert_eq!(
            turns[0].approval.as_ref().map(|a| a.answer.as_str()),
            Some("yes"),
            "审批脚本要带过去"
        );
        assert_eq!(
            turns[0]
                .approval
                .as_ref()
                .and_then(|a| a.remember.as_deref()),
            Some("session")
        );
        assert_eq!(scenario.expect.events.len(), 3);
    }

    #[test]
    fn an_unversioned_recording_is_refused() {
        let body = ONE.replace("\"schemaVersion\":1,", "");
        let (_dir, path) = tmp("old.jsonl", &body);
        let error = load_all(&[path]).expect_err("没写版本就该拒绝");
        assert!(format!("{error}").contains("schemaVersion"), "{error}");
    }

    #[test]
    fn a_replay_with_no_events_is_refused() {
        let body = ONE.replace(
            r#""expect":{"events":["turnStarted","approvalRequested","turnCompleted"]}"#,
            r#""expect":{}"#,
        );
        assert_ne!(body, ONE, "替换没生效，用例其实还带着 events");
        let (_dir, path) = tmp("empty.jsonl", &body);
        let error = load_all(&[path]).expect_err("没有事件序列就不算回放");
        assert!(format!("{error}").contains("expect.events"), "{error}");
    }
}
