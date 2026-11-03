//! `run_command` — lets Joy run commands on this machine, but every gate is visible.
//!
//! This is the highest-privilege thing in the project, so its shape is "off by default +
//! three gates + an OS-level sandbox":
//!
//! 1. **Hard-deny list**: catastrophic things never run no matter how it's configured
//!    (`sudo`, `mkfs`, fork bombs…). Not configurable — a fuse config can switch off isn't a fuse.
//! 2. **allowlist**: nothing runs unless a rule in `JOY_EXEC_ALLOW` matches. An empty
//!    allowlist = nothing may run (deny by default, not allow by default).
//! 3. **Sandbox**: the command runs inside macOS `sandbox-exec` / Linux `bubblewrap`,
//!    writes limited to the working directory and temp dirs. **No sandbox, no run** —
//!    never run a command bare "just to make it work"; that's why this module exists.
//!
//! Like the rest of the tool layer: refusals and failures are **text**, not errors. A refusal
//! must say why (which rule blocked it, how to relax it) so the model can explain to the user.
//!
//! ## Boundaries (written honestly here; don't let them quietly grow)
//!
//! * **Network off by default** (`JOY_EXEC_NETWORK=1` to allow): the sandbox really cuts it
//!   (seatbelt's `(deny network*)` / bwrap's `--unshare-net`). A behavior change: earlier
//!   versions only restricted write paths, and commands could still reach the network.
//! * Writable roots beyond working dir and home can be appended via `JOY_EXEC_WRITABLE_ROOTS`
//!   (build caches are the classic case). Nothing appended by default — fewer open dirs is better.
//! * Hard-deny is **substring matching**: fancy evasions slip past; allowlist and sandbox are the real defense.
//! * No interactive approval popup (the third gate is expressed by the allowlist).
//! * Config is read once at startup (`config/write` changes need a process restart).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

use crate::{require_str, Tool, ToolCtx};

/// Output cap. Past it, truncate and **label honestly** — a `find /` can flood the context,
/// and the model must know "this isn't everything".
const MAX_OUTPUT_CHARS: usize = 8_000;

/// The execution policy fixed at startup (from `JOY_EXEC*`).
#[derive(Debug, Clone, Default)]
pub struct ExecPolicy {
    /// Rules from `JOY_EXEC_ALLOW`. Empty = nothing may run.
    pub allow: Vec<String>,
    pub timeout_secs: i64,
    /// Whether the sandbox may use the network (`JOY_EXEC_NETWORK`). Default **false**: an
    /// allowed command shouldn't default to being able to send data out. Commands like
    /// `cargo test` that download must turn the flag on explicitly.
    pub network: bool,
    /// When the allowlist doesn't match, may we **ask once** (`JOY_APPROVAL=on-request`)?
    /// Default false = deny outright (same as the old behavior).
    pub approval: bool,
    /// Seconds to wait for a human answer (`JOY_APPROVAL_TIMEOUT`).
    pub approval_timeout_secs: i64,
    /// Where oversized output is spilled (`<home>/spill`). `None` = don't spill, just truncate.
    ///
    /// Truncation is required (a `find /` can flood the context), but **what's dropped is gone**.
    /// Spilling the original keeps it reachable for model and human alike.
    pub spill_dir: Option<PathBuf>,
    /// Extra writable roots (`JOY_EXEC_WRITABLE_ROOTS`): beyond working dir and home.
    /// Build caches are the classic case — without it, `cargo build` can't write target/ in the sandbox.
    pub extra_roots: Vec<PathBuf>,
}

/// Patterns that never run. Lowercased comparison, substring match — better to miss a
/// fancy evasion than to build a regex that hits normal commands.
const HARD_DENY: &[&str] = &[
    "sudo ",
    "sudo\t",
    "doas ",
    "mkfs",
    "dd if=",
    "dd of=",
    "shutdown",
    "reboot",
    "halt ",
    ":(){", // fork bomb
    "chmod -r 777 /",
    "chmod 777 /",
    "chown -r /",
    "> /dev/disk",
    "> /dev/sd",
    "diskutil erase",
    "launchctl unload",
    "systemctl disable",
    "rm -rf /*",
    "rm -rf ~",
    "rm -rf $home",
    "eval $(",
    "history -c",
];

/// Hand-rolled detection of "download piped straight into a shell": `curl … | sh` has a URL
/// in the middle, so literal substring matching can't see it — yet it's the classic "ran a
/// stranger's code". Split the pipe; check downloader on the left, shell on the right.
fn pipes_into_a_shell(command: &str) -> bool {
    let segments: Vec<&str> = command.split('|').map(str::trim).collect();
    segments.windows(2).any(|pair| {
        let downloads = pair[0].starts_with("curl") || pair[0].starts_with("wget");
        let runs = matches!(
            pair[1].split_whitespace().next(),
            Some("sh" | "bash" | "zsh" | "dash" | "python" | "python3" | "perl" | "ruby")
        );
        downloads && runs
    })
}

/// "Someone already said yes" — replaces the approval flow when a `PermissionRequest` hook allows.
///
/// Used in exactly one spot: the hook explicitly returned `permissionDecision: allow`, i.e.
/// someone decided for the user — don't ask again.
struct AlreadyApproved;

impl crate::approval::ApprovalBroker for AlreadyApproved {
    fn request(&self, _request: crate::approval::ApprovalRequest) -> crate::approval::ApprovalFut {
        Box::pin(async { true })
    }
}

/// The three outcomes a gate can produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// Let it through.
    Allow,
    /// Deny. The reason goes straight to the model (handed over with an `Error:` prefix).
    Deny(String),
    /// No allowlist match, but `JOY_APPROVAL=on-request` is on: ask once, then decide.
    ///
    /// **Only the allowlist gate may be asked about.** The hard-deny list and the sandbox
    /// returned before this — approval never touches them; a fuse a click can bypass isn't a fuse.
    NeedsApproval { reason: String },
}

/// Should the command run? Three gates in order; the first to stop it decides.
pub fn vet(command: &str, policy: &ExecPolicy) -> Gate {
    let lower = command.to_lowercase();

    // Gate 1: hard-deny. Not configurable, not negotiable.
    if let Some(hit) = HARD_DENY.iter().find(|pattern| lower.contains(*pattern)) {
        return Gate::Deny(format!(
            "这条命令被硬拒名单挡下了（命中 '{hit}'）—— 这类命令无论怎么配都不会执行。"
        ));
    }
    if pipes_into_a_shell(&lower) {
        return Gate::Deny(
            "这条命令被硬拒名单挡下了（把下载的内容直接交给 shell 执行）\
             —— 这类命令无论怎么配都不会执行。"
                .to_string(),
        );
    }

    // Gate 2: allowlist. Empty table = deny by default; with `on-request`, switch to "ask once".
    if policy.allow.is_empty() {
        let why = "执行工具没有放行任何命令。要允许一类命令，设 JOY_EXEC_ALLOW，\
                   例如 JOY_EXEC_ALLOW='cargo test,git status,ls *'。"
            .to_string();
        return if policy.approval {
            Gate::NeedsApproval { reason: why }
        } else {
            Gate::Deny(why)
        };
    }
    if !policy.allow.iter().any(|rule| matches_rule(rule, command)) {
        let why = format!(
            "没有匹配的放行规则。当前规则：{}。\
             要允许它，往 JOY_EXEC_ALLOW 里加一条（支持末尾的 * 通配）。",
            policy.allow.join(", ")
        );
        return if policy.approval {
            Gate::NeedsApproval { reason: why }
        } else {
            Gate::Deny(why)
        };
    }

    // Gate 3: sandbox. Unavailable means no run — this module's reason to exist, not negotiable.
    if !sandbox_available() {
        return Gate::Deny(
            "这台机器上没有可用的沙箱（macOS 需要 sandbox-exec，Linux 需要 bubblewrap），\
             拒绝在沙箱之外执行命令。"
                .to_string(),
        );
    }
    Gate::Allow
}

/// Allow-rule matching: exact string equality, or prefix match when the rule ends in `*`;
/// a lone `*` means "allow everything" (it still can't pass the hard-deny list).
fn matches_rule(rule: &str, command: &str) -> bool {
    let rule = rule.trim();
    if rule == "*" {
        return true;
    }
    match rule.strip_suffix('*') {
        Some(prefix) => command.starts_with(prefix.trim_end_matches(' ')),
        None => command.trim() == rule,
    }
}

/// Is the sandbox usable right now?
///
/// "Usable" is **tested**, not looked up: the file being there doesn't mean it runs.
/// This matters most on Linux — Ubuntu 24.04 restricts unprivileged user namespaces with
/// AppArmor by default, so `bwrap` installed there still fails with "setting up uid map:
/// Permission denied". Allow by only checking "is bwrap in PATH" and the command runs with
/// the sandbox **not in effect** — the worst thing this module could do.
pub fn sandbox_available() -> bool {
    sandbox_backend().is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sandbox {
    /// macOS: `sandbox-exec -p <profile> <shell> -c <command>`.
    Seatbelt,
    /// Linux: `bwrap … <shell> -c <command>`.
    Bubblewrap,
}

fn sandbox_backend() -> Option<Sandbox> {
    // One probe is enough: the result is cached for the process lifetime, saving a fork per command.
    static BACKEND: std::sync::OnceLock<Option<Sandbox>> = std::sync::OnceLock::new();
    *BACKEND.get_or_init(|| {
        if Path::new("/usr/bin/sandbox-exec").exists() {
            return Some(Sandbox::Seatbelt);
        }
        if which("bwrap") && bubblewrap_runs() {
            return Some(Sandbox::Bubblewrap);
        }
        None
    })
}

/// Actually launch bubblewrap once (`/bin/true`) and see whether it can build a sandbox.
fn bubblewrap_runs() -> bool {
    std::process::Command::new("bwrap")
        .args([
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--bind",
            "/tmp",
            "/tmp",
            "--die-with-parent",
            "--",
            "/bin/true",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// Keep the command's write access inside these `writable` roots (plus temp dirs).
///
/// Roots must be **canonicalized** before going into sandbox rules: on macOS `/var/folders/…`
/// is a symlink to `/private/var/folders/…`, and seatbelt wants real paths — rules written
/// with the former silently fail to allow the directory.
fn sandbox_command(
    backend: Sandbox,
    command: &str,
    writable: &[PathBuf],
    network: bool,
) -> tokio::process::Command {
    let argv = sandbox_argv(backend, command, writable, network);
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd
}

/// The sandbox command's argv.
///
/// Pulled out to be **testable**: the argument shape (especially "is the network cut")
/// shouldn't only be visible when actually run — such a "test" tests nothing.
pub(crate) fn sandbox_argv(
    backend: Sandbox,
    command: &str,
    writable: &[PathBuf],
    network: bool,
) -> Vec<String> {
    let roots: Vec<PathBuf> = writable
        .iter()
        .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
        .collect();
    match backend {
        Sandbox::Seatbelt => {
            // Deny all writes first, then open roots one by one — in SBPL later rules win.
            let mut profile = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
            for root in &roots {
                profile.push_str(&format!(
                    "(allow file-write* (subpath \"{}\"))\n",
                    root.display()
                ));
            }
            for tmp in ["/tmp", "/private/tmp"] {
                profile.push_str(&format!("(allow file-write* (subpath \"{tmp}\"))\n"));
            }
            profile.push_str(&format!(
                "(allow file-write* (subpath \"{}\"))\n",
                std::env::temp_dir().display()
            ));
            if !network {
                // seatbelt's default is "allow everything", so cutting the network needs an explicit deny.
                profile.push_str("(deny network*)\n");
            }
            vec![
                "/usr/bin/sandbox-exec".to_string(),
                "-p".to_string(),
                profile,
                "/bin/sh".to_string(),
                "-c".to_string(),
                command.to_string(),
            ]
        }
        Sandbox::Bubblewrap => {
            // Root mounted read-only; working dir and /tmp writable.
            let mut args = vec![
                "--ro-bind".to_string(),
                "/".to_string(),
                "/".to_string(),
                "--dev".to_string(),
                "/dev".to_string(),
                "--proc".to_string(),
                "/proc".to_string(),
                "--bind".to_string(),
                "/tmp".to_string(),
                "/tmp".to_string(),
                "--die-with-parent".to_string(),
            ];
            for root in &roots {
                args.push("--bind".to_string());
                args.push(root.display().to_string());
                args.push(root.display().to_string());
            }
            if !network {
                // No network namespace: nothing is reachable (harder than firewall rules —
                // it doesn't depend on kernel rule matching).
                args.push("--unshare-net".to_string());
            }
            // Only after `--` comes what to run: same shape as the probe run.
            args.push("--".to_string());
            args.push("/bin/sh".to_string());
            args.push("-c".to_string());
            args.push(command.to_string());
            let mut argv = vec!["bwrap".to_string()];
            argv.extend(args);
            argv
        }
    }
}

/// Gate + approval. `Ok(())` = may run; `Err(reason)` = don't (reason is model-readable).
///
/// **Foreground and background jobs share this spot**: a background command that could
/// bypass approval makes it theater — "background it, run at leisure" is the natural bypass.
pub async fn pass_gate(
    command: &str,
    policy: &ExecPolicy,
    approval: Option<&dyn crate::approval::ApprovalBroker>,
) -> Result<(), String> {
    match vet(command, policy) {
        Gate::Deny(why) => Err(why),
        Gate::Allow => Ok(()),
        Gate::NeedsApproval { reason } => {
            // Nobody to ask, no answer, answered too late — all denials. Approval has one source:
            // an explicit "yes".
            let Some(broker) = approval else {
                return Err(format!(
                    "{reason}\n（也没人在问你：JOY_APPROVAL=on-request 时，从终端或 \
                     驾驶舱提问才有人能回答。）"
                ));
            };
            let request = crate::approval::ApprovalRequest {
                tool: "run_command".to_string(),
                args_preview: command.to_string(),
                reason,
                timeout_secs: policy.approval_timeout_secs,
            };
            if broker.request(request).await {
                Ok(())
            } else {
                Err("这次执行没有被批准（或没人回答），已跳过。".to_string())
            }
        }
    }
}

/// Turn the policy into a command that can be **spawned directly**: sandbox args, writable-root
/// merge, and the network switch all in one spot, shared by foreground and background jobs —
/// write the sandbox args twice and one path will sooner or later forget `--unshare-net`.
pub fn spawn_sandboxed(
    command: &str,
    policy: &ExecPolicy,
    writable: &[PathBuf],
) -> Result<tokio::process::Child, String> {
    let backend = sandbox_backend().ok_or_else(|| "这台机器上没有可用的沙箱".to_string())?;

    let mut roots: Vec<PathBuf> = writable.to_vec();
    roots.extend(policy.extra_roots.iter().cloned());

    let mut command_builder = sandbox_command(backend, command, &roots, policy.network);
    command_builder
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("命令起不来：{e}"))
}

/// Run a command. **Never returns Err** — refusals, timeouts, non-zero exits are all text.
///
/// `writable` holds the roots the caller decides **dynamically** (working dir, home);
/// `policy.extra_roots` (from config) is merged in here — merging inside execute keeps
/// "what the policy says" true on every call path: scatter the merge across callers and
/// one path will silently ignore the config.
pub async fn execute(
    command: &str,
    policy: &ExecPolicy,
    writable: &[PathBuf],
    approval: Option<&dyn crate::approval::ApprovalBroker>,
) -> String {
    if let Err(why) = pass_gate(command, policy, approval).await {
        return format!("Error: {why}");
    }
    let mut child = match spawn_sandboxed(command, policy, writable) {
        Ok(child) => child,
        Err(why) => return format!("Error: {why}"),
    };

    let timeout = Duration::from_secs(policy.timeout_secs.max(1) as u64);
    let mut stdout = child.stdout.take().expect("stdout 是 piped");
    let mut stderr = child.stderr.take().expect("stderr 是 piped");

    // Read and wait share one timeout budget; on timeout, kill_on_drop cleans up. The two
    // pipes are read concurrently — stdout-then-stderr deadlocks when stderr fills the pipe.
    let collected = tokio::time::timeout(timeout, async {
        let mut out = String::new();
        let mut err = String::new();
        let (out_result, err_result) = tokio::join!(
            stdout.read_to_string(&mut out),
            stderr.read_to_string(&mut err)
        );
        let status = child.wait().await;
        (out_result, err_result, status, out, err)
    })
    .await;

    let (out_result, _err_result, status, out, err) = match collected {
        Ok(collected) => collected,
        Err(_) => {
            return format!(
                "Error: 命令超过 {} 秒还没结束，已经掐掉。（要放宽就设 JOY_EXEC_TIMEOUT）",
                policy.timeout_secs
            );
        }
    };
    if let Err(e) = out_result {
        return format!("Error: 读输出失败：{e}");
    }
    let code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);

    let mut combined = format!("退出码 {code}\n{out}");
    if !err.trim().is_empty() {
        combined.push_str("\n--- stderr ---\n");
        combined.push_str(&err);
    }
    // Too long → swap in a "fitting stub": spill the full original, keep head/tail **whole lines**
    // in context. A failed spill doesn't change the return value — a failed write shouldn't
    // cost the command's own output. Implementation shared with the in-turn budget (`crate::spill`).
    match crate::spill::stub(
        policy.spill_dir.as_deref(),
        &combined,
        MAX_OUTPUT_CHARS,
        "command",
    ) {
        Some(stub) => stub.text,
        None => combined,
    }
}

/// Clean files older than `max_age_days` from `spill/`. Runs once at startup.
///
/// Time-based only, no quota (see docs/limitations.md): a one-shot sweep is more predictable
/// than a capacity guess. Failures don't error — this is housekeeping, not a feature.
pub fn prune_spill(home: &Path, max_age_days: u64) {
    let spill = home.join("spill");
    let Ok(days) = std::fs::read_dir(&spill) else {
        return; // no spill dir is the norm
    };
    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs(max_age_days * 24 * 60 * 60);
    for day in days.flatten() {
        let path = day.path();
        if !path.is_dir() {
            continue;
        }
        let fresh = std::fs::read_dir(&path)
            .map(|files| {
                files.flatten().any(|file| {
                    file.metadata()
                        .and_then(|meta| meta.modified())
                        .map(|modified| modified > cutoff)
                        .unwrap_or(true) // unreadable time counts as fresh; don't misdelete
                })
            })
            .unwrap_or(true);
        if !fresh {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// The `run_command` registered into the tool table. Only registered when `JOY_EXEC=1` —
/// when it's off, the model can't even see this tool.
pub fn run_command(policy: ExecPolicy) -> Tool {
    let policy = Arc::new(policy);
    Tool {
        name: "run_command".to_string(),
        description: "在用户的机器上执行一条 shell 命令（受沙箱限制：只能写工作目录与临时目录）。\
                      适合查看状态、跑测试、git 只读操作这类事。\
                      命令被拒绝时会给出原因，照原因调整或请用户放开 JOY_EXEC_ALLOW。"
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "要执行的 shell 命令"},
                "cwd": {"type": "string", "description": "工作目录（默认：Joy 启动时所在目录）"},
                "background": {
                    "type": "boolean",
                    "description": "丢到后台跑（默认 false = 这一轮等它结束）。适合几分钟以上的命令：\
                                   起完立刻拿到 job id，用 job_output 看进度、job_list 看全部、\
                                   job_kill 停掉。沙箱与批准规则与前台完全一致。"
                }
            },
            "required": ["command"]
        }),
        handler: Arc::new(move |ctx: ToolCtx, args: Value| {
            let policy = policy.clone();
            Box::pin(async move {
                let command = require_str(&args, "command")?;
                let background = args
                    .get("background")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                // Writable roots: resolve chain args.cwd > ctx.cwd (this turn's working dir) >
                // process cwd, plus home (Joy's own state, like outbox, lives there).
                let cwd = args
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .filter(|p| p.is_dir())
                    .or_else(|| ctx.cwd.clone())
                    .or_else(|| std::env::current_dir().ok());
                let mut writable: Vec<PathBuf> = Vec::new();
                if let Some(cwd) = &cwd {
                    writable.push(cwd.clone());
                }
                writable.push(ctx.home.clone());

                // PermissionRequest: give the hook a say **before asking the human** — it can
                // approve on the user's behalf (no interruption), or refuse outright.
                let mut approval = ctx.approval.clone();
                if let Some(hooks) = &ctx.hooks {
                    let outcome = hooks
                        .fire(
                            crate::hooks::HookEvent::PermissionRequest,
                            serde_json::json!({
                                "session_id": ctx.session_id,
                                "cwd": cwd,
                                "tool_name": "run_command",
                                "tool_input": { "command": command },
                            }),
                        )
                        .await;
                    if let Some(why) = outcome.blocked {
                        return Ok(format!(
                            "Error: {why}（PermissionRequest hook 拒绝了这条命令）"
                        ));
                    }
                    if outcome.allowed {
                        eprintln!("(joy) PermissionRequest hook 放行了这条命令：{command}");
                        approval = Some(Arc::new(AlreadyApproved));
                    }
                }

                if background {
                    // Background goes through the same gate: skip it and "background it, run at
                    // leisure" becomes the most natural way around approval.
                    if let Err(why) = pass_gate(&command, &policy, approval.as_deref()).await {
                        return Ok(format!("Error: {why}"));
                    }
                    let Some(jobs) = ctx.jobs.clone() else {
                        return Ok(
                            "Error: 这台机器上没有开启后台作业（它跟着 run_command 一起开关：\
                             JOY_EXEC=1）。"
                                .to_string(),
                        );
                    };
                    let cwd_text = cwd.as_ref().map(|p| p.display().to_string());
                    return Ok(
                        match jobs.spawn(&ctx.session_id, &command, cwd_text).await {
                            Ok(id) => format!(
                            "已在后台跑起来：{id}（{command}）\n用 job_output 看进度、job_list \
                             看全部、job_kill 停掉。"
                        ),
                            Err(why) => format!("Error: {why}"),
                        },
                    );
                }

                // `JOY_EXEC_WRITABLE_ROOTS` is merged by execute itself (the policy holds on
                // every call path; it doesn't rely on callers remembering to merge).
                Ok(execute(&command, &policy, &writable, approval.as_deref()).await)
            })
        }),
    }
}
