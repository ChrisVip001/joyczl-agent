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
                    .ok_or_else(|| "缺少 path".to_string()),
                "pattern" => view
                    .pattern
                    .clone()
                    .filter(|text| !text.is_empty())
                    .ok_or_else(|| "缺少 pattern".to_string()),
                _ => view
                    .tool
                    .as_deref()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| "缺少 tool".to_string()),
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
            "toolsOk" => Ok(Self::ToolsOk {
                tool: field("tool")?,
                min: min(1) as u32,
            }),
            other => Err(format!(
                "不认识的验收类型 '{other}'（支持 fileExists / fileContains / toolsOk）"
            )),
        }
    }

    /// The condition in the human's words. The card shows this line; the check reports the
    /// measurement beside it.
    pub fn label(&self) -> String {
        match self {
            Self::FileExists { path } => format!("{path} 存在"),
            Self::FileContains { path, pattern, min } => {
                format!("{path} 里至少 {min} 处「{pattern}」")
            }
            Self::ToolsOk { tool, min } => format!("{tool} 成功调用至少 {min} 次"),
        }
    }
}

/// Parse a task row's `acceptance_json`. Bad entries are dropped **with a loud line** — they can
/// never pass anyway, and losing the whole list because one entry is malformed would be worse.
pub fn parse(json: &str) -> Vec<Acceptance> {
    let views: Vec<AcceptanceView> = match serde_json::from_str(json) {
        Ok(views) => views,
        Err(e) => {
            eprintln!("(joy) 验收条件读不出来（当作没有验收条件）：{e}");
            return Vec::new();
        }
    };
    let mut parsed = Vec::new();
    for (index, view) in views.iter().enumerate() {
        match Acceptance::from_view(view) {
            Ok(acceptance) => parsed.push(acceptance),
            Err(why) => eprintln!("(joy) 第 {index} 条验收条件跳过：{why}"),
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
    /// Passed: the evidence ("report.md 12.4 KB"). Failed: the measurement ("report.md 不存在").
    pub detail: String,
}

impl CheckResult {
    /// The wire shape (`CheckView`), one per condition, same order.
    pub fn view(&self) -> joyczl_protocol::CheckView {
        joyczl_protocol::CheckView {
            label: self.label.clone(),
            passed: self.passed,
            detail: self.detail.clone(),
        }
    }
}

/// Everything a check needs to look at the world. Constructed once per round.
#[derive(Clone)]
pub struct Verifier {
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
            let (passed, detail) = match acceptance {
                Acceptance::FileExists { path } => self.file_exists(path),
                Acceptance::FileContains { path, pattern, min } => {
                    self.file_contains(path, pattern, *min)
                }
                Acceptance::ToolsOk { tool, min } => self.tools_ok(tool, *min).await,
            };
            results.push(CheckResult {
                label,
                passed,
                detail,
            });
        }
        results
    }

    /// Resolve one condition's path inside the root, or say why it cannot be looked at.
    fn locate(&self, path: &str) -> Result<PathBuf, String> {
        let Some(root) = &self.root else {
            return Err("没有工作目录（cwd 为空），路径检查一律拒绝".to_string());
        };
        let candidate = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            root.join(path)
        };
        // The same rule the write tools use: one containment implementation, no second opinion.
        crate::paths::inside_workspace(&candidate, root)
    }

    fn file_exists(&self, path: &str) -> (bool, String) {
        match self.locate(path) {
            Err(why) => (false, why),
            Ok(full) => match std::fs::metadata(&full) {
                Ok(meta) if meta.is_file() => {
                    (true, format!("{} {}", path, human_bytes(meta.len())))
                }
                Ok(_) => (false, format!("{path} 存在但不是文件")),
                Err(_) => (false, format!("{path} 不存在")),
            },
        }
    }

    fn file_contains(&self, path: &str, pattern: &str, min: usize) -> (bool, String) {
        let full = match self.locate(path) {
            Ok(full) => full,
            Err(why) => return (false, why),
        };
        let text = match std::fs::read_to_string(&full) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (false, format!("{path} 不存在"))
            }
            Err(e) => return (false, format!("{path} 读不了：{e}")),
        };
        // Plain substring count, deliberately not JSON/regex aware: the condition came from a
        // human's config file, and "a broken pattern must not stall the round" wins.
        let found = text.matches(pattern).count();
        if found >= min {
            (true, format!("{path} 里找到 {found} 处（要求 {min}）"))
        } else {
            (
                false,
                format!("{path} 里只找到 {found} 处「{pattern}」（要求 {min}）"),
            )
        }
    }

    async fn tools_ok(&self, tool: &str, min: u32) -> (bool, String) {
        match self.steps.count_ok_by_tool(&self.task_id, tool).await {
            Ok(count) if count >= min as i64 => {
                (true, format!("{tool} 已成功 {count} 次（要求 {min}）"))
            }
            Ok(count) => (false, format!("{tool} 只成功 {count} 次（要求 {min}）")),
            Err(e) => (false, format!("查不到 {tool} 的执行记录：{e}")),
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
    Some(format!("验收未通过：{listed}"))
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
