//! 终端对话 —— `joy` 裸跑的那条路。
//!
//! 进程内直接持有 app-server 的 `Server`（同一个进程，state.db 照旧只开
//! 一份），跑的也是 dashboard 走的同一个 `run_turn` —— 终端、网页、网关
//! 三条路共用一个大脑，行为不会分叉。
//!
//! 不引 readline：行式 stdin 够用，方向键历史是 shell 的事。

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

/// 进入终端对话。拿到的是已装配好的 Server。
pub async fn run(server: Server) -> Result<()> {
    println!("Joy — 本地优先的个人助手。/help 看命令，/quit 退出。");
    println!();

    let mut session_id = new_session_id();
    // 本会话的工作目录（会话级 working-directory）：客户端状态，
    // 每轮 turn 都带给服务端。/cwd 命令负责校验。
    let mut cwd: Option<std::path::PathBuf> = None;
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut line = String::new();

    loop {
        print!("you> ");
        std::io::stdout().flush()?;
        line.clear();
        if tokio::io::AsyncBufReadExt::read_line(&mut stdin, &mut line).await? == 0 {
            break; // stdin 关了（Ctrl-D）
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
            s if s.starts_with("/goal") => set_goal(&server, &session_id, s),
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

/// `/cwd` —— 本会话的工作目录（会话级 working-directory）。
/// 校验在命令里做（进 turn 之前就把坏路径告诉用户），之后每轮 turn 都带上它。
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

/// `/route` —— 按任务档位自动选模型（任务档位抽象）。
///
/// 与 `/model` 同一条纪律：**只有人能到这里**。路由是服务端策略，模型没有
/// 换模型的工具；人类 pin（active profile）优先于任何档位映射。
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

/// `/model` —— 多套模型配置：列、切、加、删。
///
/// 与 `/goal` 同一条纪律：**只有人能到这里**。模型没有 `model/*` 工具，也拿不到
/// 密钥 —— 它若能让 Joy 自己换 provider，就等于能把自己接到别的服务器上。
fn models(server: &Server, input: &str) {
    let rest: Vec<&str> = input.split_whitespace().collect();
    let sub = rest.get(1).copied().unwrap_or("list");

    match sub {
        // ---- 列出 -------------------------------------------------------
        "list" | "ls" => {
            let response = server.model_profiles();
            if response.profiles.is_empty() {
                println!(
                    "还没配任何模型。用 /model add <名字> <厂商> [模型] [--env 变量名] 加一套；\
                     /model ls 之后再 /model use <名字> 切换。model/list 能看到 12 家厂商的 id。"
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

        // ---- 切换（只带名字，不重发密钥） ---------------------------------
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

        // ---- 新增/改 ------------------------------------------------------
        // /model add <名字> <厂商> [模型] [--env 变量名] [--key 密钥] [--url 端点]
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
                        // 第一个裸参数是模型名，多余的报出来而不是静默忽略。
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

        // ---- 删除 ---------------------------------------------------------
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

/// `/goal <条件>` 设一个目标，`/goal clear`（或 `/goal` 后面什么都不写）清掉。
///
/// 目标是**只有人**能设的东西：模型看不见这条路径，所以「模型给自己派活」在这里
/// 不是被禁止，而是不存在。
fn set_goal(server: &Server, session_id: &str, input: &str) {
    let condition = input.trim_start_matches("/goal").trim();
    let cleared = condition.is_empty() || condition.eq_ignore_ascii_case("clear");
    let response = server.goal_set(session_id, if cleared { None } else { Some(condition) });

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

/// 问一次批准：打印预览、读一行回答、把回答送回去。
///
/// 含混的回答（空行、乱打）一律算拒绝 —— 与整个批准机制同一条规矩：放行只有
/// 一种来源，就是一个明确的「可以」。
async fn ask_approval(server: &Server, ask: joyczl_protocol::ApprovalRequestedNotification) {
    println!("…需要批准：{}", ask.args_preview);
    println!("  为什么问：{}", ask.reason);
    print!("  批准执行？（y = 允许 / a = 允许并记住 / 其他 = 拒绝）: ");
    std::io::stdout().flush().ok();

    // 单独开一个 stdin 句柄：主循环那一刻没在读，不会打架。
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

/// 一轮对话：流式渲染文本，门与工具的动静各占一行小字。
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
    };
    let task_server = server.clone();
    let task =
        tokio::spawn(
            async move { run_turn(&task_server, params, RequestId::Number(0), &sink).await },
        );

    // 流式期间打过的字数 —— TurnCompleted 的 reply 跟它重复，别打两遍。
    let mut streamed = 0usize;
    loop {
        let Some(frame) = rx.recv().await else {
            break;
        };
        let Frame::Notification(notification) = frame else {
            continue; // turn/start 的应答本身没什么可画的
        };
        match notification {
            ServerNotification::TextDelta(delta) => {
                print!("{}", delta.delta);
                std::io::stdout().flush().ok();
                streamed += delta.delta.chars().count();
            }
            ServerNotification::TurnStarted(TurnStartedNotification { .. }) => {}
            // 门的决定值一行小字 —— 查没查记忆，用户该看得见。
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
            // 有人问「要不要执行」：这是唯一需要用户**当场**回答的通知。
            // 直接在这里读一行 stdin —— 主循环此刻正等着一轮跑完，不会跟它
            // 抢输入（为这点事做异步 stdin 竞速不值得）。
            ServerNotification::ApprovalRequested(ask) => {
                end_streamed_line(&mut streamed);
                ask_approval(server, ask).await;
            }
            // 目标循环续了一轮：说清楚「为什么还没结束」，否则用户只会看到
            // 界面又自己动了一轮。
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
            // 重试**从不静默**：与其让用户对着一个卡住的界面猜，不如说清楚
            // 「限流了，等一会儿再来一次」。
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
                    // 没走流式（快答路径）：完整回复在这儿打。
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
                // 护栏命中不是正常现象：用户该知道这一轮有一部分预算花在
                // 原地打转上了（也解释了为什么耗时更长）。
                if completed.meta.guard_hits > 0 {
                    println!(
                        "…循环护栏命中 {} 次（模型在重复调用）",
                        completed.meta.guard_hits
                    );
                }
                break;
            }
            // 图的节点事件对终端太啰嗦；triage 开着时 meta 里能看到路线。
            ServerNotification::GraphStarted(_)
            | ServerNotification::GraphNodeStarted(_)
            | ServerNotification::GraphNodeEnded(_)
            | ServerNotification::GraphEnded(_)
            | ServerNotification::Error(_) => {}
        }
    }

    // turn 失败（比如没配 key）时上面收不到 TurnCompleted —— 把错误说清楚。
    if let Err(error) = task.await.expect("turn 任务不该 panic") {
        println!("出错了：{}", error.message);
    }
    println!();
}

/// 流式文本打到一半时先换行，别让小字黏在回复上。
fn end_streamed_line(streamed: &mut usize) {
    if *streamed > 0 {
        println!();
        *streamed = 0;
    }
}

/// `/memory [词]`：不带词列最近的，带词就是搜索（facts + episodes 一起查）。
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
