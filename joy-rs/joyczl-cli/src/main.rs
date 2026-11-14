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

const USAGE: &str = "\nJoy — a local-first personal assistant\n\nUsage:\n    joy                terminal chat\n    joy gather         morning briefing (github/web/calendar/memory in one graph, draft into outbox)\n    joy schedule       resident: run jobs on the cron in skills / schedules.json\n    joy memory reindex give a vector to every fact that has none (the JOY_EMBEDDINGS=1 leg)\n    joy skill [update] list/update skills (index at <home>/skills/index.json by default)\n    joy mcp …          list MCP servers; login runs browser OAuth; serve exposes memory as an MCP server\n    joy skill …        skills: list / export --to claude,codex / install <url>\n    joy eval [path]    deterministic eval (offline 0/1, must be 100% to pass = release gate)\n    joy eval --replay  replay eval: recorded conversations + expected event sequences (reads evals/replay/)\n    joy judge [path]   one real model turn + a judge scores it (needs a key; the score never blocks a release)\n    joy bench [path]   long-task benchmark: success rate and resume rate (recorded only, never blocks)\n    joy dashboard      start the web cockpit (http://localhost:7777)\n    joy app-server     start the JSON-RPC server (stdin/stdout, newline-delimited JSON)\n    joy hub            multi-user mode: --root <dir>, one token per person in users.json,\n                       each home fully isolated (state.db/memory/model config, the lot)\n    joy hub adduser    joy hub adduser <name> --root <dir>: mint a token and write it into\n                       users.json (0600) — the token is printed once\n    joy --version      print the version\n\nEnvironment:\n    JOY_HOME           state directory, ./.joy by default (state.db, SOUL.md, traces/ live here)\n    JOY_PORT           cockpit port, 7777 by default\n    JOY_DASHBOARD_DIR  where the dashboard build lands, joy-ts/packages/dashboard/dist by default\n    JOY_GH_REPO        the repository gather's github scan reads (owner/repo)\n";

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
            "Joy does not run natively on Windows:\n  \
             · the only exec sandboxes are macOS seatbelt and Linux bubblewrap, and with the \"no sandbox, no exec\" rule\
             run_command would always refuse on Windows;\n  \
             · hooks run through sh;\n  \
             · the Windows credential store cannot read keys back, so only plaintext secrets.json is left.\n\
             Run on macOS or Linux (WSL2 is fine). The support matrix is in docs/operations.md.",
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
            // Common runtime startup owns the execution lock before recovering state.
            let mut server = joyczl_app_server::open(&settings).await?;
            // The resident process owns its background upkeep too (the cockpit connects through it).
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
            let args: Vec<String> = std::env::args().skip(2).collect();
            match args.as_slice() {
                [] => schedule::run(server).await,
                [action, name] if action == "acknowledge" => {
                    schedule::acknowledge(&server, name).await
                }
                _ => anyhow::bail!("Usage: joy schedule [acknowledge <name>]"),
            }
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
            anyhow::bail!("Unknown subcommand '{other}'\n\n{USAGE}");
        }
        None => {
            // Terminal conversation: assemble Server in-process, REPL runs the same run_turn.
            let mut settings = joyczl_config::Settings::from_env();
            settings.home = home();
            settings.ensure_home()?;
            let mut server = joyczl_app_server::open(&settings).await?;
            // The interactive process owns its own background upkeep: memory work no longer eats into the
            // tail of the user's conversation.
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
                .ok_or_else(|| anyhow::anyhow!("--root needs a directory"));
        }
    }
    std::env::var("JOY_HUB_ROOT").map_err(|_| {
        anyhow::anyhow!("Multi-user mode needs a root: joy hub --root <dir> (or set JOY_HUB_ROOT)")
    })
}

/// `joy hub adduser <name> --root <dir>`: generates a token, writes it into
/// users.json (0600). The token is printed only once — the ledger stores it.
fn hub_adduser(args: &[String]) -> anyhow::Result<()> {
    let Some(name) = args.first() else {
        anyhow::bail!("Usage: joy hub adduser <name> --root <dir>");
    };
    anyhow::ensure!(
        joyczl_ops::hub::valid_user(name),
        "User name '{name}' is not valid: lowercase letters, digits and hyphens only, at most 32 characters (the name becomes a directory)"
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
    println!("Added user {name} (root {root}). The token is shown this once:");
    println!("{token}");
    println!("Paste it into the dialog the cockpit shows on first visit. To rotate it, delete that line in users.json and adduser again.");
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
