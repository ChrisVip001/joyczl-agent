//! Terminal conversation — the path bare `joy` takes.
//!
//! Holds the app-server's `Server` in-process (same process, state.db still opened
//! exactly once), and runs the same `run_turn` the dashboard uses — terminal, web,
//! and gateway share one brain, so behavior never forks.
//!
//! No readline: line-based stdin suffices; arrow-key history is the shell's job.

use std::io::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use joyczl_app_server::{run_turn, EventSink, Frame, Server};
use joyczl_protocol::{RequestId, ServerNotification, TurnStartParams, TurnStartedNotification};

const HELP: &str = "\
命令:
  /memory [词]   看最近记住的东西（带词就是搜索）
  /sessions      列出历史会话
  /new           开一个新会话
  /quit          退出（/exit 也行）
其他任何输入都会作为消息发给 Joy。\n/goal <条件>   设一个目标（没达成会自动续轮）；/goal clear 撤掉\n/model          看已配好的模型；/model use <名字> 切换；/model add <名字> <厂商> [模型] [--env 变量] 加一套\n/cwd [路径]    看或设本会话的工作目录（/cwd clear 清除）；相对路径都按它解析\n/route          看模型路由表；/route auto|fixed 切模式；/route set <档位> <配置名> 改映射";

/// Enters the terminal conversation. Receives an already-assembled Server.
pub async fn run(server: Server) -> Result<()> {
    println!("Joy — 本地优先的个人助手。/help 看命令，/quit 退出。");
    println!();

    let mut session_id = new_session_id();
    // This session's working directory (session-level working-directory): client
    // state, passed to the server with every turn. The /cwd command validates it.
    let mut cwd: Option<std::path::PathBuf> = None;
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut line = String::new();

    loop {
        print!("you> ");
        std::io::stdout().flush()?;
        line.clear();
        if tokio::io::AsyncBufReadExt::read_line(&mut stdin, &mut line).await? == 0 {
            break; // stdin closed (Ctrl-D)
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }

        match input {
            "/quit" | "/exit" | "/q" => break,
            "/help" => println!("{HELP}"),
            "/new" => {
                session_id = new_session_id();
                println!("新会话：{session_id}");
            }
            "/sessions" => list_sessions(&server).await,
            s if s.starts_with("/memory") => show_memory(&server, s).await,
            s if s.starts_with("/goal") => set_goal(&server, &session_id, s).await,
            s if s.starts_with("/cwd") => cwd = set_cwd(cwd, s),
            s if s.starts_with("/model") => models(&server, s),
            s if s.starts_with("/route") => route_cmd(&server, s),
            s if s.starts_with('/') => {
                println!("不认识的命令 {s}。/help 看有哪些。");
            }
            message => chat_turn(&server, &session_id, cwd.as_ref(), message).await,
        }
    }
    println!("再见。");
    Ok(())
}

/// `/cwd` — this session's working directory (session-level working-directory).
/// Validated in the command (bad paths reported before the turn); every turn carries it.
fn set_cwd(cwd: Option<std::path::PathBuf>, input: &str) -> Option<std::path::PathBuf> {
    let rest = input.trim_start_matches("/cwd").trim();
    if rest.is_empty() {
        return match &cwd {
            Some(path) => {
                println!("本轮工作目录：{}", path.display());
                cwd
            }
            None => {
                println!(
                    "本轮工作目录：未设置（按 Joy 启动时所在目录）。用 /cwd <绝对路径> 设置。"
                );
                cwd
            }
        };
    }
    if rest == "clear" || rest == "reset" {
        println!("已清除本轮工作目录。");
        return None;
    }
    let path = std::path::PathBuf::from(rest);
    if !path.is_absolute() || !path.is_dir() {
        println!("'{rest}' 必须是已存在的绝对路径 —— 没有设置。");
        return cwd;
    }
    println!("本轮工作目录已设为 {}", path.display());
    Some(path)
}

/// `/route` — auto-pick a model by task tier (the tier abstraction).
///
/// Same discipline as `/model`: **only humans reach here**. Routing is server-side
/// policy; the model has no model-switching tool; a human pin (profile) wins.
fn route_cmd(server: &Server, input: &str) {
    use joyczl_config::routes::{self, RouteFile, RouteMode, TIERS};

    let home = server.settings().home;
    let mut table: RouteFile = routes::load_routes(&home);
    let rest: Vec<&str> = input.split_whitespace().collect();

    match rest.get(1).copied() {
        None => {
            println!(
                "模式：{}（{}）",
                match table.mode {
                    RouteMode::Auto => "auto",
                    RouteMode::Fixed => "fixed",
                },
                routes::routes_path(&home).display()
            );
            for tier in TIERS {
                let mapped = table.routes.get(tier);
                println!(
                    "  {tier:<10} → {}",
                    mapped.map(String::as_str).unwrap_or("（缺省链）")
                );
            }
            println!("缺省链：reasoning/fast → chat → active；改用 /route set <档位> <配置名>。");
        }
        Some("auto") | Some("fixed") => {
            table.mode = if rest[1] == "auto" {
                RouteMode::Auto
            } else {
                RouteMode::Fixed
            };
            match routes::save_routes(&home, &table) {
                Ok(()) => println!(
                    "已切到 {} —— 每轮重读，立即生效。",
                    if table.mode == RouteMode::Auto {
                        "auto"
                    } else {
                        "fixed"
                    }
                ),
                Err(why) => println!("没写进 {}：{why}", routes::routes_path(&home).display()),
            }
        }
        Some("set") => {
            let (Some(tier), Some(profile_id)) = (rest.get(2).copied(), rest.get(3).copied())
            else {
                println!("用法：/route set <fast|chat|reasoning> <配置名>（/model 看有哪些）");
                return;
            };
            if !TIERS.contains(&tier) {
                println!("档位只有三个：fast / chat / reasoning。");
                return;
            }
            let models = joyczl_config::models::load_models(&home);
            if joyczl_config::models::find(&models, profile_id).is_none() {
                println!("没有叫 '{profile_id}' 的模型配置 —— /model 看有哪些。");
                return;
            }
            table
                .routes
                .insert(tier.to_string(), profile_id.to_string());
            match routes::save_routes(&home, &table) {
                Ok(()) => println!("{tier} → {profile_id}（每轮重读，立即生效）"),
                Err(why) => println!("没写进 {}：{why}", routes::routes_path(&home).display()),
            }
        }
        Some(other) => {
            println!("不认识的子命令 '{other}'。/route 看表，/route auto|fixed 切模式，/route set <档位> <配置名> 改映射。");
        }
    }
}

/// `/model` — multiple model profiles: list, switch, add, remove.
///
/// Same discipline as `/goal`: **only humans reach here**. The model has no
/// `model/*` tools or keys — if it could rewire Joy's provider, it could rehome itself.
fn models(server: &Server, input: &str) {
    let rest: Vec<&str> = input.split_whitespace().collect();
    let sub = rest.get(1).copied().unwrap_or("list");

    match sub {
        // ---- list --------------------------------------------------------
        "list" | "ls" => {
            let response = server.model_profiles();
            if response.profiles.is_empty() {
                println!(
                    "还没配任何模型。用 /model add <名字> <厂商> [模型] [--env 变量名] 加一套；\
                     /model ls 之后再 /model use <名字> 切换。model/list 能看到全部厂商 id（含 \
                     <home>/models-catalog.json 里自加的）。"
                );
                return;
            }
            for profile in &response.profiles {
                let mark = if profile.active { "*" } else { " " };
                let model = profile.model.as_deref().unwrap_or("（厂商默认）");
                let key = profile.key_source.as_deref().unwrap_or("无需 key");
                println!(
                    " {mark} {:<12} {:<12} {:<24} key: {key}",
                    profile.id, profile.provider, model
                );
            }
            println!("（* = 当前生效）");
        }

        // ---- switch (name only, no re-sent keys) --------------------------
        "use" => {
            let Some(id) = rest.get(2) else {
                println!("用法：/model use <名字>");
                return;
            };
            match server.model_save(joyczl_protocol::ModelSaveParams {
                id: id.to_string(),
                provider: None,
                model: None,
                small_model: None,
                base_url: None,
                api_key: None,
                key_env: None,
                make_active: Some(true),
            }) {
                Ok(response) => println!(
                    "已切到 {}（{}）。",
                    response.id,
                    response.current.unwrap_or_else(|| "还没配好".to_string())
                ),
                Err(why) => println!("切不过去：{why}"),
            }
        }

        // ---- add/edit ------------------------------------------------------
        // /model add <name> <vendor> [model] [--env var] [--key key] [--url endpoint]
        "add" => {
            let id = rest.get(2).copied().unwrap_or("").to_string();
            let provider = rest.get(3).map(|s| s.to_string());
            if id.is_empty() || provider.is_none() {
                println!(
                    "用法：/model add <名字> <厂商> [模型] [--env 变量名] [--key 密钥] [--url 端点]\n\
                     例：/model add ds deepseek deepseek-chat --env DEEPSEEK_API_KEY"
                );
                return;
            }
            let mut model = None;
            let mut key_env = None;
            let mut api_key = None;
            let mut base_url = None;
            let mut positionals = 0;
            let mut i = 4;
            while i < rest.len() {
                match rest[i] {
                    "--env" => {
                        key_env = rest.get(i + 1).map(|s| s.to_string());
                        i += 2;
                    }
                    "--key" => {
                        api_key = rest.get(i + 1).map(|s| s.to_string());
                        i += 2;
                    }
                    "--url" => {
                        base_url = rest.get(i + 1).map(|s| s.to_string());
                        i += 2;
                    }
                    other => {
                        // The first bare positional is the model name; extras are reported, not dropped.
                        positionals += 1;
                        if positionals == 1 {
                            model = Some(other.to_string());
                        } else {
                            println!("多余的参数 '{other}' —— 记不住的就用 --key / --env。");
                        }
                        i += 1;
                    }
                }
            }
            match server.model_save(joyczl_protocol::ModelSaveParams {
                id,
                provider,
                model,
                small_model: None,
                base_url,
                api_key,
                key_env,
                make_active: Some(true),
            }) {
                Ok(response) => println!(
                    "已存下 {}（{}），现在是当前的。密钥存在：{}\n\
                     （keychain = 系统钥匙串；file = 明文兜底文件，见 limitations）",
                    response.id,
                    response.current.unwrap_or_else(|| "还没配好".to_string()),
                    response.key_stored_in,
                ),
                Err(why) => println!("存不进去：{why}"),
            }
        }

        // ---- remove ---------------------------------------------------------
        "rm" | "remove" => {
            let Some(id) = rest.get(2) else {
                println!("用法：/model rm <名字>");
                return;
            };
            let response =
                server.model_remove(joyczl_protocol::ModelRemoveParams { id: id.to_string() });
            if response.removed {
                println!("已删掉 {id}（连它的凭据一起）。");
            } else {
                println!("没有叫 {id} 的配置。");
            }
        }

        other => println!(
            "不认识 /model {other}。可用：ls / use <名字> / add <名字> <厂商> [模型] / rm <名字>"
        ),
    }
}

/// `/goal <condition>` sets a goal; `/goal clear` (or bare `/goal`) clears it.
///
/// Goals are **human-only**: the model cannot see this path, so "the model
/// assigning itself work" isn't forbidden here — it simply doesn't exist.
async fn set_goal(server: &Server, session_id: &str, input: &str) {
    let condition = input.trim_start_matches("/goal").trim();
    let cleared = condition.is_empty() || condition.eq_ignore_ascii_case("clear");
    let response = server
        .goal_set(session_id, if cleared { None } else { Some(condition) })
        .await;

    if !response.active {
        println!("目标已清除（这个会话现在没有目标）。");
        return;
    }
    println!(
        "目标已设置：{}\n（最多自动续 {} 轮；停下来的理由每次都会打在这里；\n  /goal clear 可以随时撤掉）",
        response.condition.unwrap_or_default(),
        response.max_rounds
    );
}

/// Asks for one approval: print a preview, read a line of answer, send it back.
///
/// Ambiguous answers (empty line, mashing keys) all count as refusal — same rule
/// as the whole approval mechanism: only one source of yes, an explicit "yes".
async fn ask_approval(server: &Server, ask: joyczl_protocol::ApprovalRequestedNotification) {
    println!("…需要批准：{}", ask.args_preview);
    println!("  为什么问：{}", ask.reason);
    print!("  批准执行？（y = 允许 / a = 允许并记住 / 其他 = 拒绝）: ");
    std::io::stdout().flush().ok();

    // A separate stdin handle: the main loop isn't reading at that moment, no fight.
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    let _ = tokio::io::AsyncBufReadExt::read_line(&mut stdin, &mut line).await;
    let answer = line.trim().to_lowercase();
    let approved = matches!(answer.as_str(), "y" | "yes" | "a" | "always");
    let remember = matches!(answer.as_str(), "a" | "always");

    let accepted = server.answer_approval(&ask.turn_id, &ask.request_id, approved, remember);
    if !accepted {
        println!("…这个问题已经过期了（那一轮不再等答案）");
    } else if approved {
        println!(
            "…批准了{}",
            if remember {
                "，并记住这条命令"
            } else {
                ""
            }
        );
    } else {
        println!("…拒绝了");
    }
}

fn new_session_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("cli-{millis}")
}

/// One chat turn: stream-render text; gate and tool activity each get a small line.
async fn chat_turn(
    server: &Server,
    session_id: &str,
    cwd: Option<&std::path::PathBuf>,
    message: &str,
) {
    let (sink, mut rx) = EventSink::channel();
    let params = TurnStartParams {
        session_id: Some(session_id.to_string()),
        message: message.to_string(),
        stream: Some(true),
        cwd: cwd.map(|p| p.display().to_string()),
        attachments: None,
        tier: None,
    };
    let task_server = server.clone();
    let task =
        tokio::spawn(
            async move { run_turn(&task_server, params, RequestId::Number(0), &sink).await },
        );

    // Chars streamed so far — TurnCompleted's reply repeats it; don't print twice.
    let mut streamed = 0usize;
    loop {
        let Some(frame) = rx.recv().await else {
            break;
        };
        let Frame::Notification(notification) = frame else {
            continue; // turn/start's own response has nothing to draw
        };
        match notification {
            ServerNotification::TextDelta(delta) => {
                print!("{}", delta.delta);
                std::io::stdout().flush().ok();
                streamed += delta.delta.chars().count();
            }
            ServerNotification::TurnStarted(TurnStartedNotification { .. }) => {}
            // Gate decision as one small line — whether memory was consulted, the user should see.
            ServerNotification::GateDecided(decided) => {
                end_streamed_line(&mut streamed);
                println!(
                    "…记忆门：{}（{}）",
                    match decided.decision.decision {
                        joyczl_protocol::GateDecisionKind::Retrieve => "检索",
                        joyczl_protocol::GateDecisionKind::Skip => "跳过",
                    },
                    decided.decision.reason
                );
            }
            // Someone asks "execute or not": the only notification needing a user
            // answer **right now**. Read a line of stdin here — the main loop is
            // waiting for the turn to finish, so no input race (async stdin racing
            // isn't worth it for this).
            ServerNotification::ApprovalRequested(ask) => {
                end_streamed_line(&mut streamed);
                ask_approval(server, ask).await;
            }
            // The goal loop continued another round: say why it hasn't ended, or the
            // user only sees the interface move by itself again.
            ServerNotification::GoalRound(round) => {
                end_streamed_line(&mut streamed);
                match round.status.as_str() {
                    "continuing" => println!(
                        "…目标还没达成（第 {}/{} 轮）：{}",
                        round.round, round.max_rounds, round.reason
                    ),
                    "satisfied" => println!("…目标达成（第 {} 轮）：{}", round.round, round.reason),
                    "impossible" => {
                        println!("…目标做不到（第 {} 轮）：{}", round.round, round.reason)
                    }
                    "round-limit" => println!(
                        "…到达轮次上限（{} 轮），停在这里：{}",
                        round.max_rounds, round.reason
                    ),
                    other => println!("…目标循环停了（{other}）：{}", round.reason),
                }
            }
            // Retries are **never silent**: rather than letting the user guess at a
            // stuck interface, say plainly "rate-limited, wait a bit and try again".
            ServerNotification::Retry(retry) => {
                end_streamed_line(&mut streamed);
                println!(
                    "…模型那边{}，{}ms 后重试（第 {} 次）",
                    retry.reason, retry.delay_ms, retry.attempt
                );
            }
            ServerNotification::ToolStarted(started) => {
                end_streamed_line(&mut streamed);
                println!("…正在调用 {}…", started.tool);
            }
            ServerNotification::ToolCompleted(done) => {
                let status = if done.status == joyczl_protocol::ToolStatus::Ok {
                    "ok"
                } else {
                    "出错"
                };
                println!(
                    "…{} 完成（{}，{}ms）",
                    done.tool,
                    status,
                    done.duration_ms.unwrap_or(0)
                );
            }
            ServerNotification::ConsolidationCompleted(done) => {
                println!("…提炼出 {} 条新事实", done.new_facts);
            }
            ServerNotification::TurnCompleted(completed) => {
                end_streamed_line(&mut streamed);
                if streamed == 0 {
                    // Not streamed (fast-answer path): the full reply prints here.
                    println!("{}", completed.reply);
                }
                let usage = completed
                    .usage
                    .map(|u| format!("token {}+{}", u.input_tokens, u.output_tokens))
                    .unwrap_or_default();
                println!(
                    "— {} · {} 轮 · {}ms{}{}",
                    completed.meta.model,
                    completed.iterations,
                    completed.meta.latency_ms,
                    if usage.is_empty() { "" } else { " · " },
                    usage,
                );
                if completed.meta.interrupted {
                    println!("（这轮被打断了。）");
                }
                // Guard hits are not normal: the user should know part of this turn's
                // budget went to spinning in place (which also explains the longer time).
                if completed.meta.guard_hits > 0 {
                    println!(
                        "…循环护栏命中 {} 次（模型在重复调用）",
                        completed.meta.guard_hits
                    );
                }
                break;
            }
            // Inbound mail: the terminal says how many arrived, not their content.
            ServerNotification::InboxReceived(inbox) => {
                end_streamed_line(&mut streamed);
                println!(
                    "…收件箱新增 {} 封（connector {}），处理方式看 connectors.json 的 inbound.mode",
                    inbox.added, inbox.connector
                );
            }
            // A long task changed state: the terminal says which way it went, never silently.
            ServerNotification::TaskUpdated(updated) => {
                end_streamed_line(&mut streamed);
                println!(
                    "…任务「{}」：{}（{} 轮）{}",
                    updated.task_id,
                    updated.status,
                    updated.rounds_used,
                    updated
                        .reason
                        .as_deref()
                        .map(|reason| format!("—— {reason}"))
                        .unwrap_or_default()
                );
            }
            // Resumed / suspended are rare and meaningful — one honest line each. A boundary
            // checkpoint (every tool round) is too chatty for the terminal; the trace has it.
            ServerNotification::TurnResumed(resumed) => {
                end_streamed_line(&mut streamed);
                println!(
                    "…从上次中断处继续（已完成 {} 轮，不重跑已完成的工具）",
                    resumed.from_iterations
                );
            }
            ServerNotification::TurnSuspended(suspended) => {
                end_streamed_line(&mut streamed);
                println!("…这个回合已暂停（{}），不会自动续跑", suspended.reason);
            }
            // Graph node events are too chatty for the terminal; with triage on, meta shows the route.
            ServerNotification::GraphStarted(_)
            | ServerNotification::GraphNodeStarted(_)
            | ServerNotification::GraphNodeEnded(_)
            | ServerNotification::GraphEnded(_)
            | ServerNotification::TurnCheckpointed(_)
            | ServerNotification::Error(_) => {}
        }
    }

    // On turn failure (e.g. no key configured) TurnCompleted never arrives above — state the error.
    if let Err(error) = task.await.expect("turn 任务不该 panic") {
        println!("出错了：{}", error.message);
    }
    println!();
}

/// When streamed text is mid-line, break first so small lines don't stick to the reply.
fn end_streamed_line(streamed: &mut usize) {
    if *streamed > 0 {
        println!();
        *streamed = 0;
    }
}

/// `/memory [word]`: no word lists recent ones; a word searches (facts + episodes).
async fn show_memory(server: &Server, input: &str) {
    let query = input.trim_start_matches("/memory").trim();
    let facts = if query.is_empty() {
        server.facts().recent(10, 0).await.unwrap_or_default()
    } else {
        server.facts().search(query, 5).await.unwrap_or_default()
    };
    let episodes = if query.is_empty() {
        server.episodes().recent(5).await.unwrap_or_default()
    } else {
        server.episodes().search(query, 3).await.unwrap_or_default()
    };

    if facts.is_empty() && episodes.is_empty() {
        println!(
            "记忆{}还是空的。告诉 Joy 一些关于你的事，它就会记住（state.db 就在 home 里）。",
            if query.is_empty() {
                String::new()
            } else {
                format!("里关于「{query}」的东西")
            }
        );
        return;
    }
    for fact in &facts {
        println!(
            "- **{}**: {}（{}）",
            fact.subject, fact.content, fact.source
        );
    }
    for episode in &episodes {
        println!("- ({}) {}", episode.happened_at, episode.summary);
    }
}

async fn list_sessions(server: &Server) {
    let sessions = server.chat().sessions().await.unwrap_or_default();
    if sessions.is_empty() {
        println!("还没有历史会话。");
        return;
    }
    for s in sessions {
        println!(
            "- {}  {}（{} 条，最近 {}）",
            s.id,
            s.title,
            s.messages,
            s.last_at.as_deref().unwrap_or("?")
        );
    }
}
