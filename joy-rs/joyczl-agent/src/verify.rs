//! Deterministic acceptance checks — "how do we know it is done" answered by the filesystem.
//!
//! The goal loop's judge is a model: it reads the conversation and says whether the goal looks
//! met. That is an opinion, and an opinion is exactly what a task must not be closed on. These
//! checks are the facts underneath it:
//!
//! * `FileExists`   — the file is really there (and how big it is);
//! * `FileContains` — a plain substring appears at least N times (no regex: a badly written
//!   pattern must not be able to stall a whole round);
//! * `ToolsOk`      — the tool really ran and succeeded N times (from the step ledger, not from
//!   the model's own summary).
//!
//! Every check is a `stat` / a read / one SQL count. No model call, no network. When a condition
//! fails the judge is **not consulted at all** — that saves a call and, more importantly, keeps
//! the model from talking itself into "done".

use std::path::{Path, PathBuf};

use joyczl_protocol::AcceptanceView;
use joyczl_state::Steps;

/// One acceptance condition, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acceptance {
    FileExists {
        path: String,
    },
    FileContains {
        path: String,
        pattern: String,
        min: usize,
    },
    JsonEquals {
        path: String,
        pointer: String,
        expected: serde_json::Value,
    },
    ToolsOk {
        tool: String,
        min: u32,
    },
}

impl Acceptance {
    /// Turn one wire-shaped condition into a checked one. A malformed condition is an error
    /// **with its index**: silently skipping it would turn "not verified" into "passed".
    pub fn from_view(view: &AcceptanceView) -> Result<Self, String> {
        let field = |name: &str| -> Result<String, String> {
            match name {
                "path" => view
                    .path
                    .as_deref()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| "missing path".to_string()),
                "pattern" => view
                    .pattern
                    .clone()
                    .filter(|text| !text.is_empty())
                    .ok_or_else(|| "missing pattern".to_string()),
                _ => view
                    .tool
                    .as_deref()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| "missing tool".to_string()),
            }
        };
        let min = |fallback: i32| -> usize { view.min.unwrap_or(fallback).max(1) as usize };
        match view.kind.as_str() {
            "fileExists" => Ok(Self::FileExists {
                path: field("path")?,
            }),
            "fileContains" => Ok(Self::FileContains {
                path: field("path")?,
                pattern: field("pattern")?,
                min: min(1),
            }),
            "jsonEquals" => {
                let pointer = view.pointer.as_deref().ok_or("missing pointer")?;
                if pointer.len() > 1024 || (!pointer.is_empty() && !pointer.starts_with('/')) {
                    return Err("JSON pointer must be empty or begin with / and fit in 1024 bytes".into());
                }
                let mut chars = pointer.chars();
                while let Some(ch) = chars.next() {
                    if ch == '~' && !matches!(chars.next(), Some('0' | '1')) {
                        return Err("JSON pointer contains an invalid escape".into());
                    }
                }
                let raw = view.expected_json.as_deref().ok_or("missing expectedJson")?;
                if raw.len() > 16_384 { return Err("expectedJson exceeds 16384 bytes".into()); }
                let expected = serde_json::from_str(raw).map_err(|_| "expectedJson must contain a valid JSON value")?;
                Ok(Self::JsonEquals { path: field("path")?, pointer: pointer.into(), expected })
            }
            "toolsOk" => Ok(Self::ToolsOk {
                tool: field("tool")?,
                min: min(1) as u32,
            }),
            other => Err(format!(
                "unknown acceptance kind '{other}' (supported: fileExists / fileContains / jsonEquals / toolsOk)"
            )),
        }
    }

    /// The condition in the human's words. The card shows this line; the check reports the
    /// measurement beside it.
    pub fn label(&self) -> String {
        match self {
            Self::FileExists { path } => format!("{path} exists"),
            Self::FileContains { path, pattern, min } => {
                format!("{path} contains at least {min} × “{pattern}”")
            }
            Self::JsonEquals { path, pointer, .. } => {
                format!("{path} {pointer} matches the expected JSON value")
            }
            Self::ToolsOk { tool, min } => format!("{tool} succeeds at least {min} times"),
        }
    }
}

/// Execution must preserve every declared condition, including after storage corruption.
pub fn parse_checked(json: &str) -> Result<Vec<Acceptance>, String> {
    let views: Vec<AcceptanceView> = serde_json::from_str(json)
        .map_err(|error| format!("Invalid task acceptance criteria: {error}"))?;
    views
        .iter()
        .enumerate()
        .map(|(index, view)| {
            Acceptance::from_view(view)
                .map_err(|reason| format!("Invalid acceptance check #{}: {reason}", index + 1))
        })
        .collect()
}

/// Parse a task row's `acceptance_json`. Bad entries are dropped **with a loud line** — they can
/// never pass anyway, and losing the whole list because one entry is malformed would be worse.
pub fn parse(json: &str) -> Vec<Acceptance> {
    let views: Vec<AcceptanceView> = match serde_json::from_str(json) {
        Ok(views) => views,
        Err(e) => {
            eprintln!("(joy) acceptance criteria could not be read (continuing as if there were none): {e}");
            return Vec::new();
        }
    };
    let mut parsed = Vec::new();
    for (index, view) in views.iter().enumerate() {
        match Acceptance::from_view(view) {
            Ok(acceptance) => parsed.push(acceptance),
            Err(why) => eprintln!("(joy) acceptance check #{index} skipped: {why}"),
        }
    }
    parsed
}

/// One condition's measured verdict.
#[derive(Debug, Clone)]
pub struct CheckResult {
    /// Human-readable condition (see `Acceptance::label`).
    pub label: String,
    pub passed: bool,
    /// Passed: the evidence ("report.md 12.4 KB"). Failed: the measurement ("report.md does not exist").
    pub detail: String,
    pub artifact: Option<joyczl_state::verification::FileMeasurement>,
}

impl CheckResult {
    /// The wire shape (`CheckView`), one per condition, same order.
    pub fn view(&self) -> joyczl_protocol::CheckView {
        joyczl_protocol::CheckView {
            label: self.label.clone(),
            passed: self.passed,
            detail: self.detail.clone(),
            artifact_sha256: self
                .artifact
                .as_ref()
                .map(|artifact| artifact.sha256.clone()),
        }
    }
}

/// Everything a check needs to look at the world. Constructed once per round.
#[derive(Clone)]
pub struct Verifier {
    pub run_id: String,
    /// The task's working directory. **`None` refuses every path check** — there is no
    /// fallback to the process cwd or the home directory: an unbounded path check is not a
    /// check, and "where did it read that from" must never be the answer.
    pub root: Option<PathBuf>,
    pub steps: Steps,
    pub task_id: String,
}

impl Verifier {
    /// Run every condition. Conditions with no filesystem/SQL access still produce a verdict
    /// (`passed: false`) with the reason — "we could not check" is never "passed".
    pub async fn run(&self, conditions: &[Acceptance]) -> Vec<CheckResult> {
        let mut results = Vec::with_capacity(conditions.len());
        for acceptance in conditions {
            let label = acceptance.label();
            let (passed, detail, artifact) = match acceptance {
                Acceptance::FileExists { path } => self.file_exists(path),
                Acceptance::FileContains { path, pattern, min } => {
                    self.file_contains(path, pattern, *min)
                }
                Acceptance::JsonEquals {
                    path,
                    pointer,
                    expected,
                } => self.json_equals(path, pointer, expected),
                Acceptance::ToolsOk { tool, min } => {
                    let (passed, detail) = self.tools_ok(tool, *min).await;
                    (passed, detail, None)
                }
            };
            results.push(CheckResult {
                label,
                passed,
                detail,
                artifact,
            });
        }
        results
    }

    /// Resolve one condition's path inside the root, or say why it cannot be looked at.
    fn locate(&self, path: &str) -> Result<PathBuf, String> {
        let Some(root) = &self.root else {
            return Err("no working directory (empty cwd) — path checks are refused".to_string());
        };
        let candidate = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            root.join(path)
        };
        // The same rule the write tools use: one containment implementation, no second opinion.
        crate::paths::inside_workspace(&candidate, root)
    }

    fn file_exists(
        &self,
        path: &str,
    ) -> (
        bool,
        String,
        Option<joyczl_state::verification::FileMeasurement>,
    ) {
        let full = match self.locate(path) {
            Ok(full) => full,
            Err(why) => return (false, why, None),
        };
        match std::fs::metadata(&full) {
            Ok(meta) if meta.is_file() => {
                match joyczl_state::verification::FileMeasurement::read(&full) {
                    Ok(measurement) => (
                        true,
                        format!("{} {}", path, human_bytes(measurement.bytes as u64)),
                        Some(measurement),
                    ),
                    Err(error) => (false, format!("Could not measure {path}: {error}"), None),
                }
            }
            Ok(_) => (false, format!("{path} exists but is not a file"), None),
            Err(_) => (false, format!("{path} does not exist"), None),
        }
    }

    fn file_contains(
        &self,
        path: &str,
        pattern: &str,
        min: usize,
    ) -> (
        bool,
        String,
        Option<joyczl_state::verification::FileMeasurement>,
    ) {
        let full = match self.locate(path) {
            Ok(full) => full,
            Err(why) => return (false, why, None),
        };
        let (text, measurement) =
            match joyczl_state::verification::FileMeasurement::read_text(&full) {
                Ok(measured) => measured,
                Err(error) => return (false, format!("Could not read {path}: {error}"), None),
            };
        let found = text.matches(pattern).count();
        let detail = if found >= min {
            format!("found {found} in {path} (needs {min})")
        } else {
            format!("only {found} × “{pattern}” in {path} (needs {min})")
        };
        (found >= min, detail, Some(measurement))
    }

    fn json_equals(
        &self,
        path: &str,
        pointer: &str,
        expected: &serde_json::Value,
    ) -> (
        bool,
        String,
        Option<joyczl_state::verification::FileMeasurement>,
    ) {
        let full = match self.locate(path) {
            Ok(full) => full,
            Err(error) => return (false, error, None),
        };
        let (text, measurement) =
            match joyczl_state::verification::FileMeasurement::read_text_limited(
                &full,
                16 * 1024 * 1024,
            ) {
                Ok(value) => value,
                Err(error) => {
                    return (
                        false,
                        format!("Could not measure JSON document {path}: {error}"),
                        None,
                    )
                }
            };
        let value: serde_json::Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(_) => {
                return (
                    false,
                    format!("{path} is not a valid JSON document"),
                    Some(measurement),
                )
            }
        };
        let found = value.pointer(pointer);
        let passed = found == Some(expected);
        let detail = match found {
            None => format!("{path} has no JSON value at {pointer}"),
            Some(_) if passed => format!("{path} {pointer} matches the expected JSON value"),
            Some(_) => format!("{path} {pointer} differs from the expected JSON value"),
        };
        (passed, detail, Some(measurement))
    }

    async fn tools_ok(&self, tool: &str, min: u32) -> (bool, String) {
        match self
            .steps
            .count_ok_by_tool_in_run(&self.task_id, &self.run_id, tool)
            .await
        {
            Ok(count) if count >= min as i64 => (
                true,
                format!("{tool} succeeded {count} times (needs {min})"),
            ),
            Ok(count) => (
                false,
                format!("{tool} only succeeded {count} times (needs {min})"),
            ),
            Err(e) => (false, format!("no execution record for {tool}: {e}")),
        }
    }
}

/// Did every condition pass? An empty list means "no objective conditions" — which the caller
/// treats as "nothing to verify", not as "passed".
pub fn all_passed(results: &[CheckResult]) -> bool {
    results.iter().all(|result| result.passed)
}

/// One sentence naming every failed condition with its measurement. This is what ends up in
/// `task.last_stop_reason` and on the card's footnote.
pub fn failure_summary(results: &[CheckResult]) -> Option<String> {
    let failed: Vec<&CheckResult> = results.iter().filter(|r| !r.passed).collect();
    if failed.is_empty() {
        return None;
    }
    let listed = failed
        .iter()
        .map(|r| r.detail.as_str())
        .collect::<Vec<_>>()
        .join("；");
    Some(format!("acceptance not met: {listed}"))
}

/// Bytes in human units. Three significant-ish digits are enough to tell 12 KB from 1 MB.
fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let value = bytes as f64;
    if value < KB {
        format!("{bytes} B")
    } else if value < KB * KB {
        format!("{:.1} KB", value / KB)
    } else {
        format!("{:.1} MB", value / (KB * KB))
    }
}
