//! `run_command` —— 让 Joy 能在这台机器上执行命令，但每一道门都看得见。
//!
//! 这是全项目权限最高的一件事，所以它的形状是「默认关闭 + 三道闸门 +
//! 操作系统级沙箱」：
//!
//! 1. **硬拒名单**：灾难性的东西无论怎么配都不跑（`sudo`、`mkfs`、fork
//!    炸弹……）。这一条不可配置 —— 一个能被配置关掉的保险丝不是保险丝。
//! 2. **allowlist**：`JOY_EXEC_ALLOW` 里没有能匹配上的规则就不跑。空
//!    allowlist = 什么都不许跑（默认拒绝，不是默认允许）。
//! 3. **沙箱**：命令跑在 macOS `sandbox-exec` / Linux `bubblewrap` 里，
//!    写权限被限制在工作目录与临时目录。**沙箱不可用就拒绝执行** ——
//!    绝不「为了跑通」裸跑一条命令，那正是这个模块存在的理由。
//!
//! 与工具层的其它成员一样：拒绝与失败都是**文本**，不是错误。拒绝原因要
//! 说清楚（哪条规则挡的、怎么放开），模型才能向用户解释为什么没做。
//!
//! ## 边界（如实写在这里，别让它悄悄扩大）
//!
//! * **默认断网**（`JOY_EXEC_NETWORK=1` 才联网）：沙箱层真的会断
//!   （seatbelt 的 `(deny network*)` / bwrap 的 `--unshare-net`）。这是
//!   行为变更：早先的版本只限制写路径，命令照样能联网。
//! * 可写根除了工作目录与 home，还可以用 `JOY_EXEC_WRITABLE_ROOTS` 追加
//!   （构建缓存是典型用例）。默认不追加 —— 开放的目录越少越好。
//! * 硬拒名单是**子串匹配**：花哨的绕法抓不住。真正的防线是放行表与沙箱。
//! * 没有交互式批准弹窗（第三道闸门用 allowlist 表达）。
//! * 只在启动时读一次配置（`config/write` 改了要重启进程才生效）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

use crate::{require_str, Tool, ToolCtx};

/// 输出上限。超过就截断并**如实标注** —— 一条 `find /` 的输出能塞爆上下文，
/// 而模型需要知道「这不是全部」。
const MAX_OUTPUT_CHARS: usize = 8_000;

/// 启动时定下的执行策略（来自 `JOY_EXEC*`）。
#[derive(Debug, Clone, Default)]
pub struct ExecPolicy {
    /// `JOY_EXEC_ALLOW` 的规则。空 = 什么都不许跑。
    pub allow: Vec<String>,
    pub timeout_secs: i64,
    /// 沙箱里能不能联网（`JOY_EXEC_NETWORK`）。默认 **false**：一条被放行的
    /// 命令默认不该拥有把数据送出去的能力。`cargo test` 这类要下载的命令
    /// 得显式把开关打开。
    pub network: bool,
    /// 放行表没匹配上时，允不允许**问一句**（`JOY_APPROVAL=on-request`）。
    /// 默认 false = 直接拒绝（等于从前行为）。
    pub approval: bool,
    /// 等人回答的秒数（`JOY_APPROVAL_TIMEOUT`）。
    pub approval_timeout_secs: i64,
    /// 超长输出的落盘位置（`<home>/spill`）。`None` = 不落盘，只截断。
    ///
    /// 截断是必须的（一条 `find /` 能塞爆上下文），但**丢掉的东西是没了**。
    /// 落一份原文件，模型与人都还能回查。
    pub spill_dir: Option<PathBuf>,
    /// 额外的可写根（`JOY_EXEC_WRITABLE_ROOTS`）：工作目录与 home 之外的。
    /// 构建缓存是典型用例 —— 没有它，`cargo build` 在沙箱里写不进 target/。
    pub extra_roots: Vec<PathBuf>,
}

/// 永远不跑的图案。小写比较，子串匹配 —— 宁可漏掉一个花哨写法，
/// 也不要做一套会误伤正常命令的正则。
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
    ":(){", // fork 炸弹
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

/// 「下载下来直接喂给 shell」的手写识别：`curl … | sh` 这类写法中间夹着
/// URL，字面子串匹配不到 —— 而它正是最经典的一条「把陌生人的代码执行了」。
/// 拆管道、看左边是不是下载器、右边是不是 shell。
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

/// 「已经有人答应了」—— `PermissionRequest` hook 放行时用它替掉批准流程。
///
/// 只在这一处用：钩子明确回了 `permissionDecision: allow`，那就是有人替用户拍了板，
/// 不该再弹一次问题。
struct AlreadyApproved;

impl crate::approval::ApprovalBroker for AlreadyApproved {
    fn request(&self, _request: crate::approval::ApprovalRequest) -> crate::approval::ApprovalFut {
        Box::pin(async { true })
    }
}

/// 闸门给出的三种结局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// 放行。
    Allow,
    /// 拒绝。理由直接给模型读（以 `Error:` 前缀交出去）。
    Deny(String),
    /// 放行表没匹配上，但开了 `JOY_APPROVAL=on-request`：问一句再决定。
    ///
    /// **只有放行表这一关可以被问到。** 硬拒名单与沙箱在这之前就返回了，
    /// 批准根本没有机会碰到它们 —— 一个能靠点「同意」绕过的保险丝不是保险丝。
    NeedsApproval { reason: String },
}

/// 命令该不该跑。三道闸门按顺序判，第一道拦下的说了算。
pub fn vet(command: &str, policy: &ExecPolicy) -> Gate {
    let lower = command.to_lowercase();

    // 闸门 1：硬拒。不可配置，也不可协商。
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

    // 闸门 2：allowlist。空表 = 默认拒绝；`on-request` 时改成「问一句」。
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

    // 闸门 3：沙箱。不可用就不跑 —— 这是本模块存在的理由，也不可协商。
    if !sandbox_available() {
        return Gate::Deny(
            "这台机器上没有可用的沙箱（macOS 需要 sandbox-exec，Linux 需要 bubblewrap），\
             拒绝在沙箱之外执行命令。"
                .to_string(),
        );
    }
    Gate::Allow
}

/// 放行规则匹配：整串相等，或规则以 `*` 结尾时的前缀匹配；`*` 单独一条
/// 表示「全部放行」（仍然过不了硬拒名单）。
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

/// 沙箱现在能不能用。
///
/// 「能用」是**测出来的**，不是查出来的：文件在那儿不等于它能跑起来。
/// 这一条在 Linux 上尤其要紧 —— Ubuntu 24.04 默认用 AppArmor 限制了非特权
/// 用户命名空间，`bwrap` 装在那儿也会以 "setting up uid map: Permission
/// denied" 失败。要是只看「bwrap 在不在 PATH 里」就放行，命令会在沙箱**没
/// 生效**的情况下跑起来，这正是本模块最不该发生的事。
pub fn sandbox_available() -> bool {
    sandbox_backend().is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sandbox {
    /// macOS：`sandbox-exec -p <profile> <shell> -c <command>`。
    Seatbelt,
    /// Linux：`bwrap … <shell> -c <command>`。
    Bubblewrap,
}

fn sandbox_backend() -> Option<Sandbox> {
    // 探测一次就够：结果缓存在进程生命周期里，省掉每条命令一次的 fork。
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

/// 真的起一次 bubblewrap（`/bin/true`），看它能不能建起沙箱。
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

/// 命令的写权限限制在 `writable` 这些根下（外加临时目录）。
///
/// 根路径要**规范化**再写进沙箱规则：macOS 上 `/var/folders/…` 是
/// `/private/var/folders/…` 的符号链接，seatbelt 认的是真实路径 —— 用前者
/// 写规则，允许的目录反而写不进去。
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

/// 沙箱命令的 argv。
///
/// 单独抽出来是为了**可测**：参数形状（尤其是「有没有断网」）不该只在真跑
/// 起来的时候才被看见 —— 那种「测试」等于没测。
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
            // 先全禁写，再逐个开回 —— SBPL 里后面的规则覆盖前面的。
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
                // seatbelt 的默认是「什么都可以」，所以断网要显式拒绝。
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
            // 根只读挂载，工作目录与 /tmp 可写。
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
                // 不给网络命名空间：连不上任何东西（比防火墙规则更硬 ——
                // 它不依赖内核的规则匹配）。
                args.push("--unshare-net".to_string());
            }
            // `--` 之后才是要跑的：与探测那一次保持同一个形状。
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

/// 执行一条命令。**永不返回 Err** —— 拒绝、超时、非零退出码都是文本。
///
/// `writable` 是调用方**动态**决定的那些根（工作目录、home）；
/// `policy.extra_roots`（配置来的）在这里一起并进去 —— 合并放在 execute 内部，
/// 是为了让「策略里写了什么」在任何调用路径上都成立：散在调用方去记得合并，
/// 就会有一条路径静默忽略配置。
pub async fn execute(
    command: &str,
    policy: &ExecPolicy,
    writable: &[PathBuf],
    approval: Option<&dyn crate::approval::ApprovalBroker>,
) -> String {
    match vet(command, policy) {
        Gate::Deny(why) => return format!("Error: {why}"),
        Gate::Allow => {}
        Gate::NeedsApproval { reason } => {
            // 没人能问、问了没人答、答得太晚 —— 都是拒绝。放行只有一种来源：
            // 一个明确的「可以」。
            let Some(broker) = approval else {
                return format!(
                    "Error: {reason}\n（也没人在问你：JOY_APPROVAL=on-request 时，从终端或 \
                     驾驶舱提问才有人能回答。）"
                );
            };
            let request = crate::approval::ApprovalRequest {
                tool: "run_command".to_string(),
                args_preview: command.to_string(),
                reason,
                timeout_secs: policy.approval_timeout_secs,
            };
            if !broker.request(request).await {
                return "Error: 这次执行没有被批准（或没人回答），已跳过。".to_string();
            }
        }
    }
    let backend = sandbox_backend().expect("vet 已经把沙箱不可用挡在外头");

    let mut roots: Vec<PathBuf> = writable.to_vec();
    roots.extend(policy.extra_roots.iter().cloned());

    let mut child = match sandbox_command(backend, command, &roots, policy.network)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return format!("Error: 命令起不来：{e}"),
    };

    let timeout = Duration::from_secs(policy.timeout_secs.max(1) as u64);
    let mut stdout = child.stdout.take().expect("stdout 是 piped");
    let mut stderr = child.stderr.take().expect("stderr 是 piped");

    // 读与等在同一个超时预算里：超时后 kill_on_drop 收尾。两个管道并发读
    // —— 先读完 stdout 再读 stderr 会在 stderr 塞满管道时互相锁死。
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
    // 超长就换成「放得下的桩」：完整原文落盘、上下文里留头尾各一半的**完整行**。
    // 落盘失败不影响返回值 —— 一次写不进磁盘不该让命令本身的输出也拿不到。
    // 实现与轮内工具结果预算共用一处（`crate::spill`）。
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

/// 清掉 `spill/` 里超过 `max_age_days` 天的文件。启动时跑一次。
///
/// 只按时间清，不做配额（见 docs/limitations.md）：一次性的清理比一个
/// 猜不准的容量上限更可预测。清不掉也不报错 —— 这是打扫，不是功能。
pub fn prune_spill(home: &Path, max_age_days: u64) {
    let spill = home.join("spill");
    let Ok(days) = std::fs::read_dir(&spill) else {
        return; // 没有 spill 目录是常态
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
                        .unwrap_or(true) // 读不出时间就当它还新，别误删
                })
            })
            .unwrap_or(true);
        if !fresh {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// 注册进工具表的 `run_command`。只有 `JOY_EXEC=1` 时才会被注册 ——
/// 没开的时候模型连这个工具都看不见。
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
                "cwd": {"type": "string", "description": "工作目录（默认：Joy 启动时所在目录）"}
            },
            "required": ["command"]
        }),
        handler: Arc::new(move |ctx: ToolCtx, args: Value| {
            let policy = policy.clone();
            Box::pin(async move {
                let command = require_str(&args, "command")?;
                // 可写的根：显式给的工作目录（或进程当前目录）+ home。
                // home 在列表里是因为 Joy 自己的状态（outbox 等）就住那儿。
                let cwd = args
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .filter(|p| p.is_dir())
                    .or_else(|| std::env::current_dir().ok());
                let mut writable: Vec<PathBuf> = Vec::new();
                if let Some(cwd) = &cwd {
                    writable.push(cwd.clone());
                }
                writable.push(ctx.home.clone());

                // PermissionRequest：在**问人之前**给钩子一次表态的机会 —— 它可以
                // 替人拍板「行」（于是不打扰用户），也可以直接拒绝。
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

                // `JOY_EXEC_WRITABLE_ROOTS` 由 execute 自己并进来（策略在
                // 任何调用路径上都生效，不靠调用方记得合并）。
                Ok(execute(&command, &policy, &writable, approval.as_deref()).await)
            })
        }),
    }
}
