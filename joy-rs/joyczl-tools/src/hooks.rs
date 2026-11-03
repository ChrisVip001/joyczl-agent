//! Hooks: run **the user's own commands** on lifecycle events.
//!
//! This is the opening for "adding behavior without changing the loop": auditing, policy
//! gates, formatting, feeding tool calls to external systems — hung on events, not a Joy fork.
//!
//! Only shell handlers (none of http / mcp_tool / prompt / agent):
//! each of those four needs its own contract, and shell already expresses them (`curl`,
//! `joy mcp`, `llm` are all one command). The un-done part is written into `docs/limitations.md`.
//!
//! Three rules, all learned from scars:
//!
//! 1. **exit 2 is the only block-via-exit-code**. exit 1 is a
//!    non-blocking error: a typo'd command must not paralyze all tools, but **must** leave a trace on stderr.
//! 2. **Policy events time out fail-closed, observation events fail-open**. `PreToolUse`
//!    is a gate — if it can't answer within the deadline, it must not default to allow;
//!    while `PostToolUse` is only a bystander; its death must not affect what already happened.
//! 3. **If the file changed, don't run it**: record the content hash at load; if it changes
//!    at runtime, refuse the new content and say why (trust hash checks). Swapping a live
//!    policy hook is "quietly changing the gate" — better to stop and be seen.
//!
//! The command receives a JSON payload (`hook_event_name` / `session_id` / `cwd` / `tool_name` /
//! `tool_input` / `tool_output`…) on stdin; to weigh in, reply with JSON on stdout:
//!
//! ```json
//! {"decision": "block", "reason": "……"}
//! {"updatedInput": {"command": "ls -la"}}
//! {"updatedOutput": "the rewritten tool result"}
//! {"additionalContext": "one line of extra context for the model"}
//! {"hookSpecificOutput": {"permissionDecision": "deny"}}
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

/// The 12 events. Names follow the ecosystem's common usage — users moving config from
/// other tools don't need to learn a second naming scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PermissionRequest,
    SessionStart,
    SessionEnd,
    Stop,
    StopFailure,
    SubagentStart,
    SubagentStop,
    PreCompact,
    PostCompact,
}

impl HookEvent {
    pub const ALL: [HookEvent; 12] = [
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::PostToolUseFailure,
        HookEvent::PermissionRequest,
        HookEvent::SessionStart,
        HookEvent::SessionEnd,
        HookEvent::Stop,
        HookEvent::StopFailure,
        HookEvent::SubagentStart,
        HookEvent::SubagentStop,
        HookEvent::PreCompact,
        HookEvent::PostCompact,
    ];

    pub fn name(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::PostToolUseFailure => "PostToolUseFailure",
            HookEvent::PermissionRequest => "PermissionRequest",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::Stop => "Stop",
            HookEvent::StopFailure => "StopFailure",
            HookEvent::SubagentStart => "SubagentStart",
            HookEvent::SubagentStop => "SubagentStop",
            HookEvent::PreCompact => "PreCompact",
            HookEvent::PostCompact => "PostCompact",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|event| event.name() == name)
    }

    /// Policy class: when it fails to answer within the deadline, treat it as **blocked** (the rest allow).
    fn is_policy(self) -> bool {
        matches!(
            self,
            HookEvent::PreToolUse | HookEvent::PermissionRequest | HookEvent::Stop
        )
    }

    /// Default timeouts. Interaction-sensitive events are shorter — a stuck `SessionStart`
    /// makes people think the program died.
    fn default_timeout(self) -> Duration {
        match self {
            HookEvent::SessionStart
            | HookEvent::SessionEnd
            | HookEvent::Stop
            | HookEvent::StopFailure
            | HookEvent::SubagentStart
            | HookEvent::SubagentStop => Duration::from_secs(30),
            _ => Duration::from_secs(600),
        }
    }
}

/// The result of one hook. A `blocked` value means block (reason shown to model/user).
#[derive(Debug, Clone, Default)]
pub struct HookOutcome {
    pub blocked: Option<String>,
    /// `PermissionRequest` only: the hook answers "yes" for the human (so no need to ask).
    pub allowed: bool,
    /// `PreToolUse` rewrites the tool's input args.
    pub input: Option<Value>,
    /// `PostToolUse` rewrites the tool's result.
    pub output: Option<String>,
    /// Extra context for the model (useful after session start, after tool results).
    pub context: Option<String>,
    /// A human-facing aside (errors, refusals…), goes to stderr.
    pub note: Option<String>,
}

impl HookOutcome {
    fn note(text: impl Into<String>) -> Self {
        Self {
            note: Some(text.into()),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone)]
struct Entry {
    /// `None`/`*` = match all; `a|b` = any; otherwise exact string equality (**no regex**:
    /// not pulling a dependency for one match; write complex checks into your script).
    matcher: Option<String>,
    command: String,
    args: Vec<String>,
    timeout: Option<Duration>,
}

impl Entry {
    fn matches(&self, subject: &str) -> bool {
        match self.matcher.as_deref() {
            None | Some("*") => true,
            Some(list) => list.split('|').any(|one| one.trim() == subject),
        }
    }
}

/// Loaded hooks. `None` means this home has none configured (no subprocess ever spawned).
pub struct Hooks {
    entries: HashMap<HookEvent, Vec<Entry>>,
    disable_all: bool,
    path: PathBuf,
    /// Content hash at load time: if it changes at runtime, don't run (see module doc rule 3).
    hash: u64,
    pub(crate) count: usize,
    /// Default timeout from config (`JOY_HOOKS_TIMEOUT`). An entry's own `timeout` wins.
    default_timeout: Option<Duration>,
}

impl Hooks {
    /// Load from `<home>/hooks.json`. Unreadable / unparseable / zero entries → `None`,
    /// with the reason on stderr (a broken config must not silently do nothing).
    pub fn load(home: &Path, default_timeout: Option<Duration>) -> Option<Self> {
        let path = home.join("hooks.json");
        let text = std::fs::read_to_string(&path).ok()?;
        let parsed: Value = match serde_json::from_str(&text) {
            Ok(parsed) => parsed,
            Err(e) => {
                eprintln!("(joy) hooks.json 解析不了（{e}）—— 这次不启用 hooks");
                return None;
            }
        };

        let disable_all = parsed
            .get("disableAllHooks")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if disable_all {
            eprintln!("(joy) hooks.json 里 disableAllHooks = true —— 一条都不跑");
            return Some(Self {
                entries: HashMap::new(),
                disable_all: true,
                path,
                hash: hash_of(&text),
                count: 0,
                default_timeout,
            });
        }

        let mut entries: HashMap<HookEvent, Vec<Entry>> = HashMap::new();
        let mut count = 0usize;
        if let Some(map) = parsed.get("hooks").and_then(Value::as_object) {
            for (name, list) in map {
                let Some(event) = HookEvent::parse(name) else {
                    eprintln!("(joy) hooks.json 里有不认识的事件 '{name}' —— 跳过");
                    continue;
                };
                let Some(list) = list.as_array() else {
                    eprintln!("(joy) hooks.json 的 '{name}' 该是一个数组 —— 跳过");
                    continue;
                };
                for raw in list {
                    let Some(command) = raw.get("command").and_then(Value::as_str) else {
                        eprintln!("(joy) hooks.json 的 '{name}' 里有一条没写 command —— 跳过");
                        continue;
                    };
                    entries.entry(event).or_default().push(Entry {
                        matcher: raw
                            .get("matcher")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        command: command.to_string(),
                        args: raw
                            .get("args")
                            .and_then(Value::as_array)
                            .map(|list| {
                                list.iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default(),
                        timeout: raw
                            .get("timeout")
                            .and_then(Value::as_u64)
                            .map(|secs| Duration::from_secs(secs.clamp(1, 300))),
                    });
                    count += 1;
                }
            }
        }

        if count == 0 {
            return None;
        }
        Some(Self {
            entries,
            disable_all: false,
            path,
            hash: hash_of(&text),
            count,
            default_timeout,
        })
    }

    /// One line for the startup log.
    pub fn summary(&self) -> String {
        if self.disable_all {
            return format!(
                "{} 里 disableAllHooks = true，一条都不跑",
                self.path.display()
            );
        }
        let mut per_event: Vec<String> = self
            .entries
            .iter()
            .map(|(event, entries)| format!("{}×{}", event.name(), entries.len()))
            .collect();
        per_event.sort();
        format!(
            "{} 条（{}），来自 {}",
            self.count,
            per_event.join("、"),
            self.path.display()
        )
    }

    /// Has the config changed at runtime? (If so, refuse to run the new content)
    fn changed(&self) -> bool {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => hash_of(&text) != self.hash,
            // File gone: counts as changed — clarity beats silently running stale content.
            Err(_) => true,
        }
    }

    /// Fire an event. **Never returns Err** — a failure is an aside, not an exception.
    pub async fn fire(&self, event: HookEvent, payload: Value) -> HookOutcome {
        let Some(entries) = self.entries.get(&event) else {
            return HookOutcome::default();
        };
        // Match subject: tool events use the tool name; other events have no natural subject —
        // session ids or custom ids in `matcher` fit poorly — so use the event name; `*`/empty matches all.
        let subject = payload
            .get("tool_name")
            .and_then(Value::as_str)
            .unwrap_or_else(|| event.name());

        let mut outcome = HookOutcome::default();
        for entry in entries {
            if !entry.matches(subject) {
                continue;
            }
            if self.changed() {
                let why = "hooks.json 在运行中被改过了 —— 这次不执行（改动可能是别的进程写的；\
                           重启 Joy 会按新内容装载）。"
                    .to_string();
                eprintln!("(joy) {why}");
                outcome.note = Some(why);
                continue;
            }
            merge(&mut outcome, self.run_one(event, entry, &payload).await);
            // Blocking short-circuits: no need to run the rest (order is priority).
            if outcome.blocked.is_some() {
                break;
            }
        }
        outcome
    }

    async fn run_one(&self, event: HookEvent, entry: &Entry, payload: &Value) -> HookOutcome {
        let mut body = payload.clone();
        if let Some(object) = body.as_object_mut() {
            object.insert("hook_event_name".to_string(), json!(event.name()));
            object.insert("matcher".to_string(), json!(entry.matcher.clone()));
        }

        let mut command = tokio::process::Command::new(&entry.command);
        command
            .args(&entry.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                // Spawn failures always fail-open: a wrong path must not paralyze all tools (but say it).
                let note = format!("hook '{}' 起不来（{e}）—— 跳过", entry.command);
                eprintln!("(joy) {note}");
                return HookOutcome::note(note);
            }
        };

        if let Some(mut stdin) = child.stdin.take() {
            let line = body.to_string();
            let _ = stdin.write_all(line.as_bytes()).await;
            let _ = stdin.write_all(b"\n").await;
            drop(stdin); // close stdin so the script knows input ended
        }

        // Priority: the entry's own timeout > the configured default (JOY_HOOKS_TIMEOUT) > event default.
        let timeout = entry
            .timeout
            .or(self.default_timeout)
            .unwrap_or_else(|| event.default_timeout());
        let waited = tokio::time::timeout(timeout, child.wait_with_output()).await;
        let output = match waited {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                let note = format!("hook '{}' 跑挂了（{e}）", entry.command);
                eprintln!("(joy) {note}");
                return HookOutcome::note(note);
            }
            Err(_) => {
                let note = format!(
                    "hook '{}' 超过 {} 秒没结束 —— {}",
                    entry.command,
                    timeout.as_secs(),
                    if event.is_policy() {
                        "策略事件，按阻断处理"
                    } else {
                        "观察事件，放行"
                    }
                );
                eprintln!("(joy) {note}");
                if event.is_policy() {
                    return HookOutcome {
                        blocked: Some(note.clone()),
                        note: Some(note),
                        ..HookOutcome::default()
                    };
                }
                return HookOutcome::note(note);
            }
        };

        let code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

        match code {
            // exit 2: the only block-via-exit-code. Reason: stdout JSON first, else stderr.
            2 => {
                let reason = json_reason(&stdout).unwrap_or_else(|| {
                    if stderr.is_empty() {
                        format!("hook '{}' 拒绝了这次动作", entry.command)
                    } else {
                        stderr.clone()
                    }
                });
                HookOutcome {
                    blocked: Some(reason),
                    ..HookOutcome::default()
                }
            }
            0 => parse_decision(&stdout, &stderr, entry, event),
            // Other exit codes: non-blocking error — note it, the action proceeds.
            other => {
                let note = format!(
                    "hook '{}' 以退出码 {other} 结束（只有 2 才是阻断）：{}",
                    entry.command,
                    if stderr.is_empty() {
                        "没有输出"
                    } else {
                        &stderr
                    }
                );
                eprintln!("(joy) {note}");
                HookOutcome::note(note)
            }
        }
    }
}

/// Parse the decision JSON on exit 0. Non-JSON is treated as a plain-text aside.
fn parse_decision(stdout: &str, stderr: &str, entry: &Entry, event: HookEvent) -> HookOutcome {
    if stderr.is_empty() && stdout.is_empty() {
        return HookOutcome::default();
    }
    let parsed = serde_json::from_str::<Value>(stdout).ok();
    let Some(value) = parsed else {
        // Plain text: context for the model (common on observation events); policy events only note on stderr.
        return HookOutcome {
            context: if stdout.is_empty() || event.is_policy() {
                None
            } else {
                Some(stdout.to_string())
            },
            note: Some(format!(
                "hook '{}'：{}",
                entry.command,
                if stderr.is_empty() { stdout } else { stderr }
            )),
            ..HookOutcome::default()
        };
    };

    let mut outcome = HookOutcome::default();
    if value.get("decision").and_then(Value::as_str) == Some("block") {
        outcome.blocked = Some(
            value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("hook 拒绝了这次动作")
                .to_string(),
        );
    }
    if let Some(input) = value.get("updatedInput").filter(|v| v.is_object()) {
        outcome.input = Some(input.clone());
    }
    if let Some(output) = value.get("updatedOutput").and_then(Value::as_str) {
        outcome.output = Some(output.to_string());
    }
    if let Some(context) = value.get("additionalContext").and_then(Value::as_str) {
        outcome.context = Some(context.to_string());
    }
    if value
        .get("hookSpecificOutput")
        .and_then(|v| v.get("permissionDecision"))
        .and_then(Value::as_str)
        == Some("allow")
    {
        outcome.allowed = true;
    }
    if value
        .get("hookSpecificOutput")
        .and_then(|v| v.get("permissionDecision"))
        .and_then(Value::as_str)
        == Some("deny")
    {
        outcome.blocked.get_or_insert_with(|| {
            value
                .get("hookSpecificOutput")
                .and_then(|v| v.get("permissionDecisionReason"))
                .and_then(Value::as_str)
                .unwrap_or("hook 拒绝了这次动作")
                .to_string()
        });
    }
    if !stderr.is_empty() {
        eprintln!("(joy) hook '{}'：{stderr}", entry.command);
    }
    outcome
}

fn json_reason(stdout: &str) -> Option<String> {
    serde_json::from_str::<Value>(stdout)
        .ok()?
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Merge one outcome into the total: **first writer wins** (order is priority).
fn merge(into: &mut HookOutcome, one: HookOutcome) {
    if into.blocked.is_none() {
        into.blocked = one.blocked;
    }
    into.allowed = into.allowed || one.allowed;
    if into.input.is_none() {
        into.input = one.input;
    }
    if into.output.is_none() {
        into.output = one.output;
    }
    if into.context.is_none() {
        into.context = one.context;
    }
    if one.note.is_some() {
        into.note = one.note;
    }
}

/// Content hash. Stdlib `DefaultHasher`: we want "did it change", not cryptographic strength.
fn hash_of(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}
