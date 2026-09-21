//! `joy` —— 命令行入口。
//!
//! `joy` 裸跑进入终端对话（进程内直接持有 app-server 的 Server）；
//! `joy app-server` 启动那个唯一持有 state.db 的进程，从 stdin 读 JSON-RPC、
//! 往 stdout 写应答；`joy dashboard` 起网页驾驶舱，它**也是** app-server 的
//! 客户端（自己 fork 一个出来说话），只是把协议翻译成了 HTTP + SSE。

mod gather;
mod mcp_cmd;
mod repl;
mod skill_cmd;

use std::path::PathBuf;

use anyhow::Result;

const USAGE: &str = "\
Joy — 本地优先的个人助手

用法:
    joy                终端对话
    joy gather         晨报（github/web/calendar/memory 一趟图跑完，草稿进 outbox）
    joy mcp …          看 MCP 服务器列表；login 跑浏览器 OAuth；serve 把记忆暴露成 MCP 服务器
    joy skill …        技能：list / export --to claude,codex / install <url>
    joy eval [路径]    确定性评测（离线 0/1，必须 100% 通过 = release gate）
    joy judge [路径]   真模型答一轮 + 裁判打分（需要 key，出分不拦发版）
    joy dashboard      起网页驾驶舱（http://localhost:7777）
    joy app-server     启动 JSON-RPC 服务端（stdin/stdout，换行分隔 JSON）
    joy --version      打印版本

环境变量:
    JOY_HOME          状态目录，默认 ./.joy（state.db、SOUL.md、traces/ 都在里面）
    JOY_PORT          驾驶舱端口，默认 7777
    JOY_DASHBOARD_DIR 前端构建产物的位置，默认 joy-ts/packages/dashboard/dist
    JOY_GH_REPO       gather 的 github scan 用的仓库（owner/repo）
";

/// 状态目录。默认放在当前工作目录下，而不是 ~/.joy ——
/// 「你的记忆就是一个能打开的文件」这句话得成立：站在哪个项目里，
/// 记忆就在哪个项目旁边。
fn home() -> PathBuf {
    std::env::var_os("JOY_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".joy"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let command = std::env::args().nth(1);
    match command.as_deref() {
        Some("mcp") => {
            let mut settings = joyczl_config::Settings::from_env();
            settings.home = home();
            let args: Vec<String> = std::env::args().skip(2).collect();
            mcp_cmd::run(&settings.home, &args).await
        }
        Some("skill") => {
            let home = home();
            let args: Vec<String> = std::env::args().skip(2).collect();
            skill_cmd::run(&home, &args).await
        }
        Some("eval") | Some("judge") => {
            let mut args: Vec<String> = std::env::args().skip(2).collect();
            // 不给路径就用仓库约定：evals/deterministic/ 与 evals/judge/。
            if args.is_empty() {
                let default = if command.as_deref() == Some("eval") {
                    "evals/deterministic"
                } else {
                    "evals/judge"
                };
                args.push(default.to_string());
            }
            let paths: Vec<PathBuf> = args.iter().map(PathBuf::from).collect();
            let report_home = home();
            let code = if command.as_deref() == Some("eval") {
                joyczl_eval::run_deterministic(&paths, &report_home).await?
            } else {
                joyczl_eval::run_judge(&paths, &report_home).await?
            };
            std::process::exit(code);
        }
        Some("app-server") => {
            // JOY_HOME 优先于环境里的 JOY_HOME —— 显式传参赢过环境变量，
            // 这样 `JOY_HOME=/tmp/x joy app-server` 和脚本里的行为可预期。
            let mut settings = joyczl_config::Settings::from_env();
            settings.home = home();
            settings.ensure_home()?;
            let server = joyczl_app_server::open(&settings).await?;
            joyczl_app_server::run_stdio(server).await
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
            // 这里不 ensure_home：驾驶舱可以比 app-server 先起来，
            // 目录由子进程建 —— 顺序反过来会先建出一个空目录，
            // 反而看不出「状态目录在哪儿」这件事其实是 app-server 决定的。
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
            // 终端对话：进程内装配 Server，REPL 跑同一个 run_turn。
            let mut settings = joyczl_config::Settings::from_env();
            settings.home = home();
            settings.ensure_home()?;
            let server = joyczl_app_server::open(&settings).await?;
            repl::run(server).await
        }
    }
}
