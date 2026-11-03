//! The shape of a deterministic case. One case = scripted model replies + assertions on harness behavior.
//!
//! Single-turn cases write `message` / `gate` / `responses` directly; cross-turn behavior
//! (sliding window, folding, consolidation, config reload, session isolation) uses `turns`.

use joyczl_provider::mock::Mock;
use joyczl_provider::CreateResponse;
use serde::Deserialize;
use serde_json::Value;

use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct Scenario {
    pub id: String,
    #[serde(default)]
    pub description: String,
    /// Single-turn convenience fields: either these or `turns`, not both.
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub gate: Option<Value>,
    #[serde(default)]
    pub responses: Vec<ScriptedResponse>,
    /// Single-turn convenience field: interrupt after the Nth streaming increment.
    #[serde(default)]
    pub interrupt_after_deltas: Option<usize>,
    /// Multi-turn: one script per turn (gate / responses / session id / interrupt / config patch).
    #[serde(default)]
    pub turns: Option<Vec<TurnScript>>,
    #[serde(default)]
    pub expect: Expectations,
    /// Overrides a few Settings knobs (sliding window, consolidate frequency, …).
    #[serde(default)]
    pub settings: SettingsOverrides,
    /// Files written into home before the run (relative paths) — for mcp.json and fake servers.
    #[serde(default)]
    pub files: Option<std::collections::BTreeMap<String, String>>,
    /// Skip this case when a prerequisite is missing ("python3" / "sh" / "sandbox"). A skip is not a failure.
    #[serde(default)]
    pub prereq: Option<String>,
}

/// The script for one turn of a multi-turn case.
#[derive(Debug, Clone, Deserialize)]
pub struct TurnScript {
    pub message: String,
    /// Session label. Unset = "eval".
    #[serde(default)]
    pub session_id: Option<String>,
    /// The retrieval gate's script. Unset = the gate gets non-JSON (fail-open tested for free).
    #[serde(default)]
    pub gate: Option<Value>,
    #[serde(default)]
    pub responses: Vec<ScriptedResponse>,
    /// A `config/write` patch applied after this turn (verifies hot config reload).
    #[serde(default)]
    pub apply_patch: Option<Value>,
    /// Interrupt this turn after the Nth streaming increment (verifies turn/interrupt).
    #[serde(default)]
    pub interrupt_after_deltas: Option<usize>,
}

/// Case-level overrides of Settings knobs.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SettingsOverrides {
    #[serde(default)]
    pub history_turns: Option<i32>,
    #[serde(default)]
    pub consolidate_every: Option<i32>,
    #[serde(default)]
    pub retrieval_top_k: Option<i32>,
    #[serde(default)]
    pub max_iterations: Option<i32>,
    #[serde(default)]
    pub max_tokens: Option<i32>,
    /// Approximate context window override — token-triggered compaction must be pinnable in a case.
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub compact_threshold: Option<f64>,
    #[serde(default)]
    pub graph_workflows: Option<bool>,
    /// Exec tool toggle and allow rules (see joyczl-tools' exec.rs).
    #[serde(default)]
    pub exec_enabled: Option<bool>,
    /// Subagents (`JOY_DELEGATE`).
    #[serde(default)]
    pub delegate_enabled: Option<bool>,
    #[serde(default)]
    pub exec_allow: Option<Vec<String>>,
    /// Lifecycle hooks (`JOY_HOOKS`).
    #[serde(default)]
    pub hooks_enabled: Option<bool>,
    /// In-turn tool result budget (see joyczl-loop's budget.rs).
    #[serde(default)]
    pub tool_result_total_chars: Option<i64>,
    #[serde(default)]
    pub tool_result_max_chars: Option<i64>,
}

impl Scenario {
    /// Expands to per-turn scripts: `turns` wins; otherwise one turn is synthesized from the single-turn fields.
    pub fn effective_turns(&self) -> Vec<TurnScript> {
        if let Some(turns) = &self.turns {
            return turns.clone();
        }
        vec![TurnScript {
            message: self.message.clone().unwrap_or_default(),
            session_id: None,
            gate: self.gate.clone(),
            responses: self.responses.clone(),
            apply_patch: None,
            interrupt_after_deltas: self.interrupt_after_deltas,
        }]
    }

    /// The mock reply queue = all turn scripts flattened in execution order (gate first,
    /// replies after). Extra model calls like consolidation are inserted in time order —
    /// case authors just follow run_turn's real order. Uniform streaming: interrupt cases
    /// depend on increments; others don't (assertions read the assembled full text). An
    /// unset `gate` auto-pads a non-JSON reply — the gate fails open without swallowing
    /// the loop's reply (the default semantics: "no gate written → fail-open tested").
    pub fn build_mock(&self, home: &std::path::Path) -> Mock {
        let mut responses: Vec<CreateResponse> = Vec::new();
        for turn in self.effective_turns() {
            let gate = match &turn.gate {
                Some(gate) => serde_json::to_string(gate).expect("gate 序列化"),
                None => "（场景没给 gate 脚本 —— 门会失败开放）".to_string(),
            };
            responses.push(Mock::text(&gate));
            for (index, response) in turn.responses.iter().enumerate() {
                responses.push(response.to_create_response(index, home));
            }
        }
        Mock::streaming(responses)
    }
}

/// The model's next move: either plain text or one tool call.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptedResponse {
    Text(String),
    ToolUse { name: String, args: Value },
}

impl ScriptedResponse {
    pub(crate) fn to_create_response(
        &self,
        index: usize,
        home: &std::path::Path,
    ) -> CreateResponse {
        match self {
            ScriptedResponse::Text(text) => Mock::text(text),
            ScriptedResponse::ToolUse { name, args } => {
                // {{home}} in tool arguments becomes the case's own home — file fixtures
                // (scenario.files) live in home, and the script must be able to point at
                // them. Expansion per `expand_home`: forward slashes, so args stay valid JSON.
                let replaced = crate::expand_home(&args.to_string(), home);
                let args: Value = serde_json::from_str(&replaced).unwrap_or_else(|error| {
                    eprintln!(
                        "(joy) 用例的工具参数解析失败（{error}），按原文送给模型：{replaced}"
                    );
                    args.clone()
                });
                Mock::tool_use(&format!("tu_{index}"), name, args)
            }
        }
    }
}

/// The assertion set. All checks are "must contain / must not contain / sequence
/// equals" grade — every deterministic eval verdict must be reproducible, never
/// resting on another model.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Expectations {
    #[serde(default)]
    pub reply_contains: Vec<String>,
    #[serde(default)]
    pub reply_not_contains: Vec<String>,
    /// The **ordered** sequence of tool names (empty vec = unchecked; accumulated across turns).
    #[serde(default)]
    pub tools_used: Vec<String>,
    /// Fragments any tool output must contain — separates "called but failed" from "really ran through".
    #[serde(default)]
    pub tool_output_contains: Vec<String>,
    /// `"retrieve"` or `"skip"` (checked every turn; any mismatch fails).
    #[serde(default)]
    pub gate: Option<String>,
    #[serde(default)]
    pub iterations: Option<i32>,
    /// Total new facts across emitted consolidation notifications.
    #[serde(default)]
    pub consolidation_new_facts: Option<i32>,
    /// The last turn was interrupted.
    #[serde(default)]
    pub interrupted: Option<bool>,
    // ---- prompt assertions (what the harness actually showed the model) ----
    /// Appeared in the full text of every model request.
    #[serde(default)]
    pub prompt_contains: Vec<String>,
    /// Appeared in none of the model requests' full text.
    #[serde(default)]
    pub prompt_not_contains: Vec<String>,
    /// Appeared in the **last** model request (asks "what did the newest turn see").
    #[serde(default)]
    pub last_prompt_contains: Vec<String>,
    /// Did not appear in the last model request.
    #[serde(default)]
    pub last_prompt_not_contains: Vec<String>,
}

/// A judge case: a real model answers one turn, the referee scores by criteria. No
/// scripted replies — the real Joy is under test.
#[derive(Debug, Clone, Deserialize)]
pub struct JudgeScenario {
    pub id: String,
    #[serde(default)]
    pub description: String,
    pub message: String,
    pub judge: JudgeSpec,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JudgeSpec {
    /// What this case cares about most (a custom section of the referee's rubric).
    pub criteria: String,
}

/// Reads judge cases (same JSONL shape as deterministic cases; directories and files both accepted).
pub fn load_judge(paths: &[PathBuf]) -> anyhow::Result<Vec<JudgeScenario>> {
    let mut scenarios = Vec::new();
    for path in paths {
        let files: Vec<PathBuf> = if path.is_dir() {
            std::fs::read_dir(path)?
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
                .collect()
        } else {
            vec![path.clone()]
        };
        for file in files {
            let text = std::fs::read_to_string(&file)?;
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                scenarios.push(serde_json::from_str(line)?);
            }
        }
    }
    Ok(scenarios)
}

/// Reads all deterministic cases: each path is a JSONL file or directory (a directory consumes its *.jsonl).
pub fn load_all(paths: &[PathBuf]) -> anyhow::Result<Vec<Scenario>> {
    let mut scenarios = Vec::new();
    for path in paths {
        let files: Vec<PathBuf> = if path.is_dir() {
            std::fs::read_dir(path)?
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
                .collect()
        } else {
            vec![path.clone()]
        };
        for file in files {
            let text = std::fs::read_to_string(&file)?;
            for (line_number, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let scenario: Scenario = serde_json::from_str(line).map_err(|e| {
                    anyhow::anyhow!(
                        "{} 第 {} 行不是合法用例：{e}",
                        file.display(),
                        line_number + 1
                    )
                })?;
                scenarios.push(scenario);
            }
        }
    }
    Ok(scenarios)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_in_tool_args_stays_valid_json_for_windows_paths() {
        // Windows homes carry backslashes. A bare replacement turns tool arguments into
        // invalid JSON, so fixture paths silently degrade to the literal {{home}} —
        // read_file / list_dir then report "file not found" on their own fixture.
        let home = std::path::Path::new(r"C:\Users\joy\AppData\Local\Temp\.tmp42");
        let scripted = ScriptedResponse::ToolUse {
            name: "read_file".to_string(),
            args: serde_json::json!({"path": "{{home}}/notes/article.md"}),
        };

        let response = scripted.to_create_response(0, home);
        let uses = response.tool_uses();
        let (_, name, input) = uses[0];
        let path = input["path"].as_str().unwrap_or_default();

        assert_eq!(name, "read_file");
        assert!(
            !path.contains("{{home}}"),
            "占位符没被展开 —— 多半是展开结果让 JSON 解析失败、退回原文了：{path}"
        );
        assert!(
            !path.contains('\\'),
            "装置路径里不该有反斜杠：它在 JSON 字符串里是非法转义：{path}"
        );
        assert!(
            path.starts_with("C:/Users/joy/AppData/Local/Temp/.tmp42/"),
            "展开的应是同一个 home 下的文件：{path}"
        );
        assert!(path.ends_with("/notes/article.md"), "{path}");
    }
}
