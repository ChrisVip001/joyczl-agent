//! `joy` — command-line entry point.
//!
//! Bare `joy` runs the terminal conversation (holding the app-server's Server
//! in-process); `joy app-server` starts the sole process that owns state.db,
//! reading JSON-RPC from stdin and writing responses to stdout; `joy dashboard`
//! starts the web cockpit, also an app-server client (it forks one to talk to),
//! just translating the protocol into HTTP + SSE.

mod connector_cmd;
mod gather;
mod mcp_cmd;
mod memory_cmd;
mod repl;
mod schedule;
mod skill_cmd;

use std::path::PathBuf;

use anyhow::Result;

const USAGE: &str = "\
Joy — 本地优先的个人助手

用法:
    joy                终端对话
    joy gather         晨报（github/web/calendar/memory 一趟图跑完，草稿进 outbox）
    joy schedule       常驻：按技能/ schedules.json 里的 cron 定时跑任务
    joy memory reindex 给还没有向量的事实补上向量（JOY_EMBEDDINGS=1 那条腿）
    joy skill [update] 列出/更新技能（索引默认 <home>/skills/index.json）
    joy mcp …          看 MCP 服务器列表；login 跑浏览器 OAuth；serve 把记忆暴露成 MCP 服务器
    joy skill …        技能：list / export --to claude,codex / install <url>
    joy eval [路径]    确定性评测（离线 0/1，必须 100% 通过 = release gate）
    joy eval --replay  回放评测：录制对话 + 期望事件序列（默认读 evals/replay/）
    joy judge [路径]   真模型答一轮 + 裁判打分（需要 key，出分不拦发版）
    joy bench [路径]   长任务基准：成功率与中断续跑率（只记录，不拦发版）
    joy dashboard      起网页驾驶舱（http://localhost:7777）
    joy app-server     启动 JSON-RPC 服务端（stdin/stdout，换行分隔 JSON）
    joy hub            多用户模式：--root <目录>，users.json 里每人一枚 token，
                       各自的 home 完整隔离（state.db/记忆/模型配置全套）
    joy hub adduser    joy hub adduser <名字> --root <目录>：生成 token 并写进
                       users.json（0600）—— token 只打印这一次
    joy --version      打印版本

环境变量:
    JOY_HOME          状态目录，默认 ./.joy（state.db、SOUL.md、traces/ 都在里面）
    JOY_PORT          驾驶舱端口，默认 7777
    JOY_DASHBOARD_DIR 前端构建产物的位置，默认 joy-ts/packages/dashboard/dist
    JOY_GH_REPO       gather 的 github scan 用的仓库（owner/repo）
";

/// State directory. Defaults to under the current working directory rather than
/// ~/.joy — "your memory is a file you can open" must hold true: whichever
/// project you stand in, memory lives next to that project.
fn home() -> PathBuf {
    std::env::var_os("JOY_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".joy"))
}

/// Why Windows cannot boot Joy, if it is Windows. The sandbox has no Windows backend
/// (and per the standing rule, **no sandbox, no execution**), hooks run through `sh`,
/// and Windows has no credential store that reads secrets back — booting would produce
/// a half-working assistant while looking perfectly healthy. Say no instead.
/// Unix-only is a stated boundary (docs/operations.md), not a silent limitation.
fn windows_blocker() -> Option<&'static str> {
    if cfg!(windows) {
        Some(
            "Joy 暂不支持在 Windows 原生运行：\n  \
             · 执行沙箱只有 macOS seatbelt / Linux bubblewrap 两个后端，按「无沙箱不执行」的铁律，\
             run_command 在 Windows 上永远拒绝执行；\n  \
             · hooks 通过 sh 执行；\n  \
             · Windows 凭据库读不回密钥，只能落明文 secrets.json。\n\
             请在 macOS 或 Linux 上运行（WSL2 即可）。支持矩阵见 docs/operations.md。",
        )
    } else {
        None
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if let Some(why) = windows_blocker() {
        eprintln!("(joy) {why}");
        std::process::exit(2);
    }
    let command = std::env::args().nth(1);
    match command.as_deref() {
        Some("mcp") => {
            let mut settings = joyczl_config::Settings::from_env();
            settings.home = home();
            let args: Vec<String> = std::env::args().skip(2).collect();
            mcp_cmd::run(&settings.home, &args).await
        }
        Some("connector") => {
            let home = home();
            let args: Vec<String> = std::env::args().skip(2).collect();
            connector_cmd::run(&home, &args).await
        }
        Some("skill") => {
            let home = home();
            let args: Vec<String> = std::env::args().skip(2).collect();
            skill_cmd::run(&home, &args).await
        }
        // Long-task benchmarks: same scripted harness as `eval`, different question — "is the
        // autonomous task work getting better or worse". Records success + recovery rates and
        // never blocks a release (exit 0). Cases live in evals/long/.
        Some("bench") => {
            let mut args: Vec<String> = std::env::args().skip(2).collect();
            if args.is_empty() {
                args.push("evals/long".to_string());
            }
            let paths: Vec<PathBuf> = args.iter().map(PathBuf::from).collect();
            let code = joyczl_eval::run_long(&paths, &home()).await?;
            std::process::exit(code);
        }
        Some("eval") | Some("judge") => {
            let mut args: Vec<String> = std::env::args().skip(2).collect();
            // `joy eval --replay [path]`: same command, different suite. A flag rather than a new
            // subcommand because everything downstream (temp home, scoring, report) is identical —
            // the only difference is which loader feeds the runner.
            let replay = command.as_deref() == Some("eval")
                && matches!(args.first().map(String::as_str), Some("--replay"));
            if replay {
                args.remove(0);
            }
            // No path given: fall back to the repo conventions.
            if args.is_empty() {
                let default = match (command.as_deref(), replay) {
                    (Some("eval"), true) => "evals/replay",
                    (Some("eval"), false) => "evals/deterministic",
                    _ => "evals/judge",
                };
                args.push(default.to_string());
            }
            let paths: Vec<PathBuf> = args.iter().map(PathBuf::from).collect();
            let report_home = home();
            let code = match (command.as_deref(), replay) {
                (Some("eval"), true) => joyczl_eval::run_replay(&paths, &report_home).await?,
                (Some("eval"), false) => {
                    joyczl_eval::run_deterministic(&paths, &report_home).await?
                }
                _ => joyczl_eval::run_judge(&paths, &report_home).await?,
            };
            std::process::exit(code);
        }
        Some("hub") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            // joy hub adduser <name> --root <dir>
            if args.first().map(String::as_str) == Some("adduser") {
                return hub_adduser(&args[1..]);
            }
            // joy hub --root <dir>
            let root = hub_root(&args)?;
            let mut settings = joyczl_config::Settings::from_env();
            settings.home = home();
            settings.ensure_home()?;
            joyczl_ops::serve_hub(
                std::path::Path::new(&root),
                &settings.ops_host,
                settings.hub_max_open as usize,
                settings.hub_idle_secs as u64,
            )
            .await
        }
        Some("app-server") => {
            // JOY_HOME here takes precedence over the env's JOY_HOME — explicit args
            // win over the env var, so `JOY_HOME=/tmp/x joy app-server` is predictable.
            let mut settings = joyczl_config::Settings::from_env();
            settings.home = home();
            settings.ensure_home()?;
            // Daemon lock: the resident process's in-memory state (approvals/goals/
            // jobs) only makes sense as one copy — two app-servers writing the same
            // state.db is an incident. REPL/cockpit don't lock (they're another path,
            // REPL holds its own in-process, see module header comment).
            let _daemon =
                joyczl_app_server::lock::acquire(&settings.home).map_err(|e| anyhow::anyhow!(e))?;
            let mut server = joyczl_app_server::open(&settings).await?;
            // 常驻进程同样自己拥有后台整理（驾驶舱经它连上来）。
            server.attach_memory_worker();
            joyczl_app_server::run_stdio(server).await
        }
        Some("memory") => {
            let mut settings = joyczl_config::Settings::from_env();
            settings.home = home();
            settings.ensure_home()?;
            let args: Vec<String> = std::env::args().skip(2).collect();
            memory_cmd::run(&settings, &args).await
        }
        Some("schedule") => {
            let mut settings = joyczl_config::Settings::from_env();
            settings.home = home();
            settings.ensure_home()?;
            let server = joyczl_app_server::open(&settings).await?;
            schedule::run(server).await
        }
        Some("gather") => {
            let mut settings = joyczl_config::Settings::from_env();
            settings.home = home();
            settings.ensure_home()?;
            let server = joyczl_app_server::open(&settings).await?;
            gather::run(&server).await
        }
        Some("dashboard") => {
            let mut settings = joyczl_config::Settings::from_env();
            settings.home = home();
            // No ensure_home here: the cockpit may start before app-server, and the
            // directory is created by the child process — reversed order would create
            // an empty dir first, obscuring that app-server really decides where the
            // state directory lives.
            joyczl_ops::serve(settings).await
        }
        Some("--version") | Some("-V") => {
            println!("joy {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => {
            anyhow::bail!("未知子命令 '{other}'\n\n{USAGE}");
        }
        None => {
            // Terminal conversation: assemble Server in-process, REPL runs the same run_turn.
            let mut settings = joyczl_config::Settings::from_env();
            settings.home = home();
            settings.ensure_home()?;
            let mut server = joyczl_app_server::open(&settings).await?;
            // 交互进程自己拥有后台整理：记忆工作不再占用户对话的收尾时间。
            server.attach_memory_worker();
            repl::run(server).await
        }
    }
}

/// `--root` argument of `joy hub` (falls back to JOY_HUB_ROOT, then errors).
fn hub_root(args: &[String]) -> anyhow::Result<String> {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--root" {
            return args
                .get(i + 1)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("--root 后面要跟目录"));
        }
    }
    std::env::var("JOY_HUB_ROOT").map_err(|_| {
        anyhow::anyhow!("多用户模式要一个根目录：joy hub --root <目录>（或设 JOY_HUB_ROOT）")
    })
}

/// `joy hub adduser <name> --root <dir>`: generates a token, writes it into
/// users.json (0600). The token is printed only once — the ledger stores it.
fn hub_adduser(args: &[String]) -> anyhow::Result<()> {
    let Some(name) = args.first() else {
        anyhow::bail!("用法：joy hub adduser <名字> --root <目录>");
    };
    anyhow::ensure!(
        joyczl_ops::hub::valid_user(name),
        "用户名 '{name}' 不合格：只许小写字母、数字与连字符，≤32 字符（名字会变成目录）"
    );
    let root = hub_root(&args[1..])?;
    let path = std::path::Path::new(&root).join("users.json");
    let mut users = joyczl_ops::hub::Users::load(&path)?;
    let token = joyczl_ops::hub::generate_token()?;
    users.add(name, &token);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    users.save(&path)?;
    println!("已加用户 {name}（根目录 {root}）。token 只显示这一次：");
    println!("{token}");
    println!("驾驶舱首次访问时把它填进弹框即可。换一枚：删掉 users.json 里那行再 adduser。");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_blocker_matches_the_platform() {
        // The blocker must agree with the platform it runs on: silent on Unix,
        // refusing on Windows. The previous version asserted "never fires" and
        // failed the Windows CI job by its own admission.
        assert_eq!(super::windows_blocker().is_none(), !cfg!(windows));
    }
}
