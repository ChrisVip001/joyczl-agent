//! Hooks：在生命周期事件上跑**用户自己的命令**。
//!
//! 这是「不改循环就能加行为」的那个口子：审计、策略门禁、格式化、把工具调用
//! 喂给外部系统 —— 都挂在事件上，而不是去 fork 一份 Joy。
//!
//! 只做 shell handler（不抄 Claude Code 的 http / mcp_tool / prompt / agent 四类）：
//! 那四类各自要一套契约，而 shell 已经能表达它们（`curl`、`joy mcp`、`llm` 都是
//! 一条命令）。少做的那部分写进了 `docs/limitations.md`。
//!
//! 三条规矩，都是从踩过的坑里来的：
//!
//! 1. **exit 2 是唯一靠退出码阻断的方式**（与 Claude Code 同口径）。exit 1 是
//!    非阻塞错误：命令写错了不该让所有工具瘫痪，但**必须**在 stderr 留下痕迹。
//! 2. **策略事件超时 fail-closed、观察事件 fail-open**（抄 hermes）。`PreToolUse`
//!    是一个闸门 —— 它没能在时限内表态，就不该默认放行；而 `PostToolUse` 只是
//!    旁观者，挂了不该影响已经发生的事。
//! 3. **文件被改过就不执行**：装载时记下内容哈希，运行中一旦变了就拒绝执行新的
//!    内容并说明原因（对齐 codex 的 `trusted_hash`）。改掉一个正在生效的策略钩
//!    是那种「悄悄换了闸门」的事，宁可停下来让人看见。
//!
//! 命令收到 JSON 载荷（`hook_event_name` / `session_id` / `cwd` / `tool_name` /
//! `tool_input` / `tool_output`…）在 stdin 上；要表态就回一段 JSON 到 stdout：
//!
//! ```json
//! {"decision": "block", "reason": "……"}
//! {"updatedInput": {"command": "ls -la"}}
//! {"updatedOutput": "改写后的工具结果"}
//! {"additionalContext": "给模型补一句背景"}
//! {"hookSpecificOutput": {"permissionDecision": "deny"}}
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

/// 12 个事件。名字与 Claude Code / codex 保持一致 —— 用户从别的 harness 抄配置
/// 过来时不必学第二套命名。
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

    /// 策略类：它没能在时限内表态时**按阻断**处理（其余按放行）。
    fn is_policy(self) -> bool {
        matches!(
            self,
            HookEvent::PreToolUse | HookEvent::PermissionRequest | HookEvent::Stop
        )
    }

    /// 默认超时。交互敏感的事件短一些 —— 一个卡住的 `SessionStart` 会让人以为
    /// 程序死了。
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

/// 一次 hook 的结果。`blocked` 有值就是阻断（理由给模型/用户看）。
#[derive(Debug, Clone, Default)]
pub struct HookOutcome {
    pub blocked: Option<String>,
    /// `PermissionRequest` 专用：钩子替人回答「行」（于是不必再去问）。
    pub allowed: bool,
    /// `PreToolUse` 改写工具入参。
    pub input: Option<Value>,
    /// `PostToolUse` 改写工具结果。
    pub output: Option<String>,
    /// 给模型补的上下文（会话开始、工具结果之后都用得上）。
    pub context: Option<String>,
    /// 给人看的旁注（出错、拒绝执行之类），进 stderr。
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
    /// `None`/`*` = 都匹配；`a|b` = 任一；其余按整串相等（**不做正则**：不为一处
    /// 匹配再引一个依赖，要更复杂的判断写进脚本里）。
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

/// 装载好的 hooks。`None` 表示这个 home 没配（没配就一次都不起子进程）。
pub struct Hooks {
    entries: HashMap<HookEvent, Vec<Entry>>,
    disable_all: bool,
    path: PathBuf,
    /// 装载时的内容哈希：运行中变了就不执行（见模块文档第 3 条）。
    hash: u64,
    pub(crate) count: usize,
    /// 来自配置的默认超时（`JOY_HOOKS_TIMEOUT`）。条目自己写了 `timeout` 就以它为准。
    default_timeout: Option<Duration>,
}

impl Hooks {
    /// 从 `<home>/hooks.json` 装载。读不到 / 解析不了 / 一条都没配 → `None`，
    /// 并把原因写到 stderr（配置写错了不该静默地什么都不做）。
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

    /// 启动日志用的一句话。
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

    /// 配置在运行中被改过吗？（改了就拒绝执行新的内容）
    fn changed(&self) -> bool {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => hash_of(&text) != self.hash,
            // 文件没了：也算变了 —— 说清楚比默默继续跑旧内容好。
            Err(_) => true,
        }
    }

    /// 触发一个事件。**永不返回 Err** —— 失败是一条旁注，不是一个异常。
    pub async fn fire(&self, event: HookEvent, payload: Value) -> HookOutcome {
        let Some(entries) = self.entries.get(&event) else {
            return HookOutcome::default();
        };
        // 命中的对象：工具类看工具名，其余事件没有可匹配的主体，`matcher` 里写
        // 会话或自定义标识都不合适 —— 用事件名本身，`*`/空 matcher 一律命中。
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
            // 阻断是短路：后面几条不必再跑（顺序即优先级）。
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
                // 起不来一律 fail-open：路径写错不该让所有工具瘫痪（但要说出来）。
                let note = format!("hook '{}' 起不来（{e}）—— 跳过", entry.command);
                eprintln!("(joy) {note}");
                return HookOutcome::note(note);
            }
        };

        if let Some(mut stdin) = child.stdin.take() {
            let line = body.to_string();
            let _ = stdin.write_all(line.as_bytes()).await;
            let _ = stdin.write_all(b"\n").await;
            drop(stdin); // 关掉 stdin，脚本才知道读完了
        }

        // 优先级：条目自己的 timeout > 配置里的默认（JOY_HOOKS_TIMEOUT）> 事件默认。
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
            // exit 2：唯一靠退出码阻断的方式。理由优先取 stdout 的 JSON，否则 stderr。
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
            // 其它退出码：非阻塞错误 —— 记下来，动作照常。
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

/// 解析 exit 0 时的决策 JSON。不是 JSON 就当纯文本旁注。
fn parse_decision(stdout: &str, stderr: &str, entry: &Entry, event: HookEvent) -> HookOutcome {
    if stderr.is_empty() && stdout.is_empty() {
        return HookOutcome::default();
    }
    let parsed = serde_json::from_str::<Value>(stdout).ok();
    let Some(value) = parsed else {
        // 纯文本：给模型当上下文（观察事件常见），策略事件只在 stderr 里提示。
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

/// 把一次结果并进总结果：**先到者优先**（顺序即优先级，与 Claude Code 同）。
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

/// 内容哈希。用标准库的 `DefaultHasher`：这里要的是「变没变」，不是密码学强度。
fn hash_of(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}
