//! Observe what a round actually did — and catch the tool that says it wrote a file that is not
//! there.
//!
//! The loop hands back a `LoopResult`: the reply, the tool outcomes (name + output only) and the
//! whole working memory. `ToolOutcome` deliberately carries no arguments, so the arguments are
//! read from the `ToolUse` blocks in `messages` and paired with their `ToolResult` by id. That
//! keeps `joyczl-loop` untouched — the loop's job is to run tools, not to audit them.
//!
//! Three products, all cheap (a few `stat`s, one INSERT batch, no model call):
//!
//! * **steps** — one per tool call, with `<turn_id>:<seq>` as the idempotency key. A resumed
//!   turn re-observes the same memory and writes the same keys: no duplicate rows, no replayed
//!   side effects.
//! * **artifacts** — files that really exist after the round, with their sizes.
//! * **contradictions** — "the tool said it wrote X, X is not there". This is the only signal
//!   that can stop a task from being marked done on a lie.

use joyczl_provider::{ContentBlock, Message, Role};
use joyczl_state::{Artifacts, Events, NewStep, Steps};

/// Tools whose arguments name a file they claim to have written.
const WRITE_TOOLS: [&str; 2] = ["write_file", "edit"];

/// What the filesystem says about one claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fact {
    Wrote {
        path: String,
        bytes: u64,
    },
    Missing {
        path: String,
    },
    /// The tool said it succeeded and the fact disagrees — the one signal that must not be
    /// talked away.
    Contradiction {
        path: String,
        said: String,
    },
}

/// One tool call, ready to be written to the step ledger.
#[derive(Debug, Clone)]
pub struct StepDraft {
    pub seq: i32,
    pub tool: String,
    pub input_json: Option<String>,
    pub ok: bool,
    /// Short digest of the output — never the whole thing (context and disk both have limits).
    pub digest: String,
    pub idempotency_key: String,
}

/// Everything one round left behind.
#[derive(Debug, Clone, Default)]
pub struct Observation {
    pub steps: Vec<StepDraft>,
    pub facts: Vec<Fact>,
}

impl Observation {
    /// The next position in the round's step sequence (1-based, monotonically increasing).
    pub fn next_seq(&self) -> i32 {
        self.steps.last().map(|step| step.seq + 1).unwrap_or(1)
    }

    /// Append another observation's steps after this one's, renumbering them. Used when one
    /// round is made of several loop passes (Stop-hook continuations) — the round keeps a single
    /// continuous step order.
    ///
    /// The idempotency key is renumbered with the step: `<turn>:<seq>` must stay in step with
    /// the row, otherwise two different steps could collide on one key (the whole point of the
    /// key is that the same step is recognised as the same step).
    pub fn absorb(&mut self, other: Observation) {
        for (seq, mut step) in (self.next_seq()..).zip(other.steps) {
            step.idempotency_key = match step.idempotency_key.rsplit_once(':') {
                Some((turn, _)) => format!("{turn}:{seq}"),
                None => step.idempotency_key,
            };
            step.seq = seq;
            self.steps.push(step);
        }
        self.facts.extend(other.facts);
    }

    /// Files that really exist, with their sizes.
    pub fn artifacts(&self) -> Vec<(String, u64)> {
        self.facts
            .iter()
            .filter_map(|fact| match fact {
                Fact::Wrote { path, bytes } => Some((path.clone(), *bytes)),
                _ => None,
            })
            .collect()
    }

    /// "The tool said it wrote, the file is not there" — one sentence per case.
    pub fn contradictions(&self) -> Vec<String> {
        self.facts
            .iter()
            .filter_map(|fact| match fact {
                Fact::Contradiction { path, said } => Some(format!(
                    "工具说写好了 {path}，但文件不存在（工具原话：{said}）"
                )),
                _ => None,
            })
            .collect()
    }

    /// Whether the round contains a lie about a side effect. The turn reads
    /// `contradictions()` for the sentences; this is the cheap boolean the tests assert on.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn has_contradiction(&self) -> bool {
        self.facts
            .iter()
            .any(|fact| matches!(fact, Fact::Contradiction { .. }))
    }
}

/// Read one loop result into steps + facts. `base_seq` lets several passes of the same round
/// share one continuous numbering.
pub fn observe(
    messages: &[Message],
    turn_id: &str,
    base_seq: i32,
    cwd: Option<&std::path::Path>,
) -> Observation {
    // Results first: the assistant's ToolUse blocks carry nothing about what happened.
    let mut results: Vec<(String, String)> = Vec::new();
    for message in messages {
        for block in &message.content {
            if let ContentBlock::ToolResult {
                tool_use_id,
                content,
            } = block
            {
                results.push((tool_use_id.clone(), content.clone()));
            }
        }
    }

    let mut observation = Observation::default();
    let mut seq = base_seq.max(1);
    for message in messages {
        if message.role != Role::Assistant {
            continue;
        }
        for block in &message.content {
            let ContentBlock::ToolUse {
                id, name, input, ..
            } = block
            else {
                continue;
            };
            let output = results
                .iter()
                .find(|(use_id, _)| use_id == id)
                .map(|(_, content)| content.as_str())
                .unwrap_or("(没有结果：这一轮在工具返回前就结束了)");
            // Same rule the loop uses to report success: output starting with "Error:" failed.
            let ok = !output.starts_with("Error:");

            let step = StepDraft {
                seq,
                tool: name.clone(),
                input_json: serde_json::to_string(input).ok(),
                ok,
                digest: digest(output),
                idempotency_key: format!("{turn_id}:{seq}"),
            };

            if WRITE_TOOLS.contains(&name.as_str()) {
                if let Some(path) = input.get("path").and_then(|value| value.as_str()) {
                    observation.facts.push(check_written(path, ok, output, cwd));
                }
            }

            observation.steps.push(step);
            seq += 1;
        }
    }
    observation
}

/// Does the file a write tool claims to have written actually exist?
fn check_written(path: &str, ok: bool, output: &str, cwd: Option<&std::path::Path>) -> Fact {
    let full = match cwd {
        Some(root) if !std::path::Path::new(path).is_absolute() => root.join(path),
        _ => std::path::PathBuf::from(path),
    };
    match std::fs::metadata(&full) {
        Ok(meta) if meta.is_file() => Fact::Wrote {
            path: full.display().to_string(),
            bytes: meta.len(),
        },
        // A failed write that left no file is honest — nothing to report.
        _ if !ok => Fact::Missing {
            path: full.display().to_string(),
        },
        _ => Fact::Contradiction {
            path: full.display().to_string(),
            said: digest(output),
        },
    }
}

/// First line, whitespace-collapsed, capped — a digest, not the output.
fn digest(output: &str) -> String {
    let one_line = output.split_whitespace().collect::<Vec<_>>().join(" ");
    let capped: String = one_line.chars().take(120).collect();
    if one_line.chars().count() > 120 {
        format!("{capped}…")
    } else {
        capped
    }
}

/// Writes one observation into the ledger. Every failure is a line on stderr: bookkeeping must
/// never take a turn down with it.
pub struct Recorder<'a> {
    pub steps: &'a Steps,
    pub artifacts: &'a Artifacts,
    pub events: &'a Events,
    pub task_id: &'a str,
    pub run_id: Option<&'a str>,
}

impl Recorder<'_> {
    pub async fn record(&self, observation: &Observation) {
        for step in &observation.steps {
            match self
                .steps
                .begin(NewStep {
                    task_id: self.task_id,
                    run_id: self.run_id,
                    seq: step.seq,
                    kind: "tool",
                    tool: Some(&step.tool),
                    input_json: step.input_json.as_deref(),
                    idempotency_key: Some(&step.idempotency_key),
                })
                .await
            {
                Ok(id) => {
                    let status = if step.ok { "ok" } else { "failed" };
                    if let Err(e) = self.steps.close(id, status, Some(&step.digest), None).await {
                        eprintln!("(joy) 步骤收尾没记上（不影响回合）：{e}");
                    }
                }
                Err(e) => eprintln!("(joy) 步骤没记进台账：{e}"),
            }
        }
        for (path, bytes) in observation.artifacts() {
            if let Err(e) = self
                .artifacts
                .add(self.task_id, self.run_id, &path, bytes as i64)
                .await
            {
                eprintln!("(joy) 交付物没记上：{e}");
            }
        }
        for contradiction in observation.contradictions() {
            eprintln!("(joy) 任务 {} 发现矛盾：{contradiction}", self.task_id);
            if let Err(e) = self
                .events
                .append(
                    self.task_id,
                    self.run_id,
                    "contradiction",
                    Some(&contradiction),
                )
                .await
            {
                eprintln!("(joy) 矛盾事件没记上：{e}");
            }
        }
    }
}
