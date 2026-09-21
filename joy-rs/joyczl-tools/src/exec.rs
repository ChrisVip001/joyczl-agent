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
//! ## v1 的边界（如实写在这里，别让它悄悄扩大）
//!
//! * 限制的是**写路径**，不是网络：命令仍然能联网。要断网得各平台再写
//!   一层（macOS seatbelt 的 network 规则 / bwrap 的 --unshare-net）。
//! * 没有交互式批准：第三道闸门用 allowlist 表达，不做「弹窗问用户」——
//!   那需要协议与 UI 一起改，是独立的一件事。
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

/// 命令该不该跑：``Ok(())`` 放行，``Err(为什么)`` 是给模型读的拒因。
pub fn vet(command: &str, policy: &ExecPolicy) -> Result<(), String> {
    let lower = command.to_lowercase();

    // 闸门 1：硬拒。不可配置。
    if let Some(hit) = HARD_DENY.iter().find(|pattern| lower.contains(*pattern)) {
        return Err(format!(
            "这条命令被硬拒名单挡下了（命中 '{hit}'）—— 这类命令无论怎么配都不会执行。"
        ));
    }
    if pipes_into_a_shell(&lower) {
        return Err(
            "这条命令被硬拒名单挡下了（把下载的内容直接交给 shell 执行）\
             —— 这类命令无论怎么配都不会执行。"
                .to_string(),
        );
    }

    // 闸门 2：allowlist。空表 = 默认拒绝。
    if policy.allow.is_empty() {
        return Err(
            "执行工具没有放行任何命令。要允许一类命令，设 JOY_EXEC_ALLOW，\
             例如 JOY_EXEC_ALLOW='cargo test,git status,ls *'。"
                .to_string(),
        );
    }
    if !policy.allow.iter().any(|rule| matches_rule(rule, command)) {
        return Err(format!(
            "没有匹配的放行规则，拒绝执行。当前规则：{}。\
             要允许它，往 JOY_EXEC_ALLOW 里加一条（支持末尾的 * 通配）。",
            policy.allow.join(", ")
        ));
    }

    // 闸门 3：沙箱。不可用就不跑 —— 这是本模块存在的理由。
    if !sandbox_available() {
        return Err(
            "这台机器上没有可用的沙箱（macOS 需要 sandbox-exec，Linux 需要 bubblewrap），\
             拒绝在沙箱之外执行命令。"
                .to_string(),
        );
    }
    Ok(())
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
enum Sandbox {
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
) -> tokio::process::Command {
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
            let mut cmd = tokio::process::Command::new("/usr/bin/sandbox-exec");
            cmd.arg("-p")
                .arg(profile)
                .arg("/bin/sh")
                .arg("-c")
                .arg(command);
            cmd
        }
        Sandbox::Bubblewrap => {
            // 根只读挂载，工作目录与 /tmp 可写。
            let mut cmd = tokio::process::Command::new("bwrap");
            cmd.arg("--ro-bind")
                .arg("/")
                .arg("/")
                .arg("--dev")
                .arg("/dev")
                .arg("--proc")
                .arg("/proc")
                .arg("--bind")
                .arg("/tmp")
                .arg("/tmp")
                .arg("--die-with-parent");
            for root in &roots {
                cmd.arg("--bind").arg(root).arg(root);
            }
            // `--` 之后才是要跑的：与探测那一次保持同一个形状。
            cmd.arg("--").arg("/bin/sh").arg("-c").arg(command);
            cmd
        }
    }
}

/// 执行一条命令。**永不返回 Err** —— 拒绝、超时、非零退出码都是文本。
pub async fn execute(command: &str, policy: &ExecPolicy, writable: &[PathBuf]) -> String {
    if let Err(why) = vet(command, policy) {
        return format!("Error: {why}");
    }
    let backend = sandbox_backend().expect("vet 已经把沙箱不可用挡在外头");

    let mut child = match sandbox_command(backend, command, writable)
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
    truncate(combined)
}

fn truncate(mut text: String) -> String {
    if text.chars().count() <= MAX_OUTPUT_CHARS {
        return text;
    }
    let cut: String = text.chars().take(MAX_OUTPUT_CHARS).collect();
    text = cut;
    text.push_str("\n…（输出太长，已截断）");
    text
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
                Ok(execute(&command, &policy, &writable).await)
            })
        }),
    }
}
