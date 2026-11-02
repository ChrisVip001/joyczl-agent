//! `joy` —— 命令行入口。
//!
//! `joy` 裸跑进入终端对话（进程内直接持有 app-server 的 Server）；
//! `joy app-server` 启动那个唯一持有 state.db 的进程，从 stdin 读 JSON-RPC、
//! 往 stdout 写应答；`joy dashboard` 起网页驾驶舱，它**也是** app-server 的
//! 客户端（自己 fork 一个出来说话），只是把协议翻译成了 HTTP + SSE。

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
    joy judge [路径]   真模型答一轮 + 裁判打分（需要 key，出分不拦发版）
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
        Some("hub") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            // joy hub adduser <名字> --root <目录>
            if args.first().map(String::as_str) == Some("adduser") {
                return hub_adduser(&args[1..]);
            }
            // joy hub --root <目录>
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
            // JOY_HOME 优先于环境里的 JOY_HOME —— 显式传参赢过环境变量，
            // 这样 `JOY_HOME=/tmp/x joy app-server` 和脚本里的行为可预期。
            let mut settings = joyczl_config::Settings::from_env();
            settings.home = home();
            settings.ensure_home()?;
            // 守护锁：常驻进程的内存态（审批/目标/作业）只有一份才有意义，
            // 两个 app-server 写同一个 state.db 是事故。REPL/驾驶舱不拿锁 ——
            // 它们是另一条路（REPL 进程内自持，见模块头注释）。
            let _daemon =
                joyczl_app_server::lock::acquire(&settings.home).map_err(|e| anyhow::anyhow!(e))?;
            let server = joyczl_app_server::open(&settings).await?;
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

/// `joy hub` 的 `--root` 参数（不给就用 JOY_HUB_ROOT，再不给就报错）。
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

/// `joy hub adduser <名字> --root <目录>`：生成 token，写进 users.json（0600）。
/// token 只打印这一次 —— 账本里存的就是它，丢了就删掉重加。
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
