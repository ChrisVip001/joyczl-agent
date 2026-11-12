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

const HELP: &str = "\nCommands:\n  /memory [term]  see what was remembered recently (with a term: search it)\n  /compact [focus] fold earlier turns into the summary now (the last two turns stay in context)\n  /sessions      list past sessions\n  /new           start a new session\n  /quit          quit (/exit works too)\nAny other input is sent to Joy as a message.\n/goal <condition>   set a goal (it auto-continues while unmet); /goal clear removes it\n/model          see the configured models; /model use <name> switches; /model add <name> <provider> [model] [--env VAR] [--vision] adds one\n/cwd [path]     show or set this session's working directory (/cwd clear clears it); relative paths resolve against it\n/route          see the model routing table; /route auto|fixed switches mode; /route set <tier> <profile> edits the mapping";

/// Enters the terminal conversation. Receives an already-assembled Server.
pub async fn run(server: Server) -> Result<()> {
    println!("Joy — a local-first personal assistant. /help for commands, /quit to leave.");
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
                println!("New session: {session_id}");
            }
            "/sessions" => list_sessions(&server).await,
            s if s.starts_with("/memory") => show_memory(&server, s).await,
            s if s.starts_with("/compact") => compact(&server, &session_id, s).await,
            s if s.starts_with("/goal") => set_goal(&server, &session_id, s).await,
            s if s.starts_with("/cwd") => cwd = set_cwd(cwd, s),
            s if s.starts_with("/model") => models(&server, s),
            s if s.starts_with("/route") => route_cmd(&server, s),
            s if s.starts_with('/') => {
                println!("Unknown command {s}. /help lists them.");
            }
            message => chat_turn(&server, &session_id, cwd.as_ref(), message).await,
        }
    }
    // A memory run in flight gets a few seconds; whatever it does not finish stays
    // claimed and the next process picks it up (see memory_worker.rs).
    server.stop_memory_worker().await;
    println!("Bye.");
    Ok(())
}

/// `/cwd` — this session's working directory (session-level working-directory).
/// Validated in the command (bad paths reported before the turn); every turn carries it.
fn set_cwd(cwd: Option<std::path::PathBuf>, input: &str) -> Option<std::path::PathBuf> {
    let rest = input.trim_start_matches("/cwd").trim();
    if rest.is_empty() {
        return match &cwd {
            Some(path) => {
                println!("This turn's working directory: {}", path.display());
                cwd
            }
            None => {
                println!(
                    "This turn's working directory: unset (Joy's own directory when it started). Set one with /cwd <absolute path>."
                );
                cwd
            }
        };
    }
    if rest == "clear" || rest == "reset" {
        println!("Cleared this turn's working directory.");
        return None;
    }
    let path = std::path::PathBuf::from(rest);
    if !path.is_absolute() || !path.is_dir() {
        println!("'{rest}' must be an existing absolute path — nothing was set.");
        return cwd;
    }
    println!("This turn's working directory is now {}", path.display());
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
                "Mode: {} ({})",
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
                    mapped.map(String::as_str).unwrap_or("(default chain)")
                );
            }
            println!("Default chain: reasoning/fast → chat → active; change it with /route set <tier> <profile>.");
        }
        Some("auto") | Some("fixed") => {
            table.mode = if rest[1] == "auto" {
                RouteMode::Auto
            } else {
                RouteMode::Fixed
            };
            match routes::save_routes(&home, &table) {
                Ok(()) => println!(
                    "Switched to {} — re-read every turn, effective immediately.",
                    if table.mode == RouteMode::Auto {
                        "auto"
                    } else {
                        "fixed"
                    }
                ),
                Err(why) => println!(
                    "Could not write {}: {why}",
                    routes::routes_path(&home).display()
                ),
            }
        }
        Some("set") => {
            let (Some(tier), Some(profile_id)) = (rest.get(2).copied(), rest.get(3).copied())
            else {
                println!("Usage: /route set <fast|chat|reasoning> <profile> (/model lists them)");
                return;
            };
            if !TIERS.contains(&tier) {
                println!("There are only three tiers: fast / chat / reasoning.");
                return;
            }
            let models = joyczl_config::models::load_models(&home);
            if joyczl_config::models::find(&models, profile_id).is_none() {
                println!("There is no model profile named '{profile_id}' — /model lists them.");
                return;
            }
            table
                .routes
                .insert(tier.to_string(), profile_id.to_string());
            match routes::save_routes(&home, &table) {
                Ok(()) => {
                    println!("{tier} → {profile_id} (re-read every turn, effective immediately)")
                }
                Err(why) => println!(
                    "Could not write {}: {why}",
                    routes::routes_path(&home).display()
                ),
            }
        }
        Some(other) => {
            println!("Unknown subcommand '{other}'. /route shows the table, /route auto|fixed switches mode, /route set <tier> <profile> edits the mapping.");
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
                    "No model is configured yet. Add one with /model add <name> <provider> [model] [--env VAR];\n                     then /model ls and /model use <name> to switch. model/list shows every provider id (including \
                     ones added in <home>/models-catalog.json)."
                );
                return;
            }
            for profile in &response.profiles {
                let mark = if profile.active { "*" } else { " " };
                let model = profile.model.as_deref().unwrap_or("(provider default)");
                let key = profile.key_source.as_deref().unwrap_or("no key needed");
                // `null` is not "no images": it means "whatever the vendor's default does".
                let vision = match profile.vision {
                    Some(true) => "vision",
                    Some(false) => "text only",
                    None => "follow the provider",
                };
                println!(
                    " {mark} {:<12} {:<12} {:<24} {:<10} key: {key}",
                    profile.id, profile.provider, model, vision
                );
            }
            println!("(* = in effect)");
        }

        // ---- switch (name only, no re-sent keys) --------------------------
        "use" => {
            let Some(id) = rest.get(2) else {
                println!("Usage: /model use <name>");
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
                // Switching must not rewrite what this profile can see.
                vision: None,
                make_active: Some(true),
            }) {
                Ok(response) => println!(
                    "Switched to {} ({}).",
                    response.id,
                    response.current.unwrap_or_else(|| "not configured yet".to_string())
                ),
                Err(why) => println!("Could not switch: {why}"),
            }
        }

        // ---- add/edit ------------------------------------------------------
        // /model add <name> <vendor> [model] [--env var] [--key key] [--url endpoint]
        "add" => {
            let id = rest.get(2).copied().unwrap_or("").to_string();
            let provider = rest.get(3).map(|s| s.to_string());
            if id.is_empty() || provider.is_none() {
                println!(
                    "Usage: /model add <name> <provider> [model] [--env VAR] [--key KEY] [--url endpoint] \
                     [--vision|--no-vision]\n\
                     e.g. /model add ds deepseek deepseek-v4.1-flash --env DEEPSEEK_API_KEY --vision"
                );
                return;
            }
            let mut model = None;
            let mut key_env = None;
            let mut api_key = None;
            let mut base_url = None;
            // `None` = follow the vendor default; `--vision` = the vendor's default model is
            // text-only but this one sees. Declared while adding, not discovered via an error.
            let mut vision = None;
            let mut positionals = 0;
            let mut i = 4;
            while i < rest.len() {
                match rest[i] {
                    "--vision" => {
                        vision = Some(true);
                        i += 1;
                    }
                    "--no-vision" => {
                        vision = Some(false);
                        i += 1;
                    }
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
                            println!("Unexpected argument '{other}' — for anything else use --key / --env.");
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
                vision,
                make_active: Some(true),
            }) {
                Ok(response) => println!(
                    "Saved {} ({}), now in effect. The key is stored in: {}\n\
                     (keychain = the system keychain; file = plaintext fallback, see limitations)",
                    response.id,
                    response.current.unwrap_or_else(|| "not configured yet".to_string()),
                    response.key_stored_in,
                ),
                Err(why) => println!("Could not store it: {why}"),
            }
        }

        // ---- remove ---------------------------------------------------------
        "rm" | "remove" => {
            let Some(id) = rest.get(2) else {
                println!("Usage: /model rm <name>");
                return;
            };
            let response =
                server.model_remove(joyczl_protocol::ModelRemoveParams { id: id.to_string() });
            if response.removed {
                println!("Deleted {id} (its credentials too).");
            } else {
                println!("There is no profile named {id}.");
            }
        }

        other => println!(
            "Unknown /model {other}. Options: ls / use <name> / add <name> <provider> [model] / rm <name>"
        ),
    }
}

/// `/goal <condition>` attaches the condition to this session's task (creating the task if
/// there is none); `/goal clear` (or bare `/goal`) takes it off, and the task stays behind as a
/// single-turn one.
///
/// Goals are **human-only**: the model cannot see this path, so "the model
/// assigning itself work" isn't forbidden here — it simply doesn't exist. There is no separate
/// goal record any more (0.17): the condition is a column on the task row, so the card and the
/// loop read exactly what this command wrote.
async fn set_goal(server: &Server, session_id: &str, input: &str) {
    let condition = input.trim_start_matches("/goal").trim();
    let cleared = condition.is_empty() || condition.eq_ignore_ascii_case("clear");
    let row = server
        .set_task_goal(session_id, if cleared { None } else { Some(condition) })
        .await;

    let Some(row) = row.filter(|row| row.goal_condition.is_some()) else {
        println!("Goal cleared (this session has no goal now).");
        return;
    };
    println!(
        "Goal set: {}\n(up to {} automatic continuations; the reason it stops is printed here every time;\n  /goal clear removes it at any point)",
        row.goal_condition.unwrap_or_default(),
        row.max_rounds
    );
}

/// Asks for one approval: print a preview, read a line of answer, send it back.
///
/// Ambiguous answers (empty line, mashing keys) all count as refusal — same rule
/// as the whole approval mechanism: only one source of yes, an explicit "yes".
async fn ask_approval(server: &Server, ask: joyczl_protocol::ApprovalRequestedNotification) {
    println!("…needs approval: {}", ask.args_preview);
    println!("  why it asks: {}", ask.reason);
    print!("  Run it? (y = this once / s = always allow in this session / a = remember this one / anything else = deny):");
    std::io::stdout().flush().ok();

    // A separate stdin handle: the main loop isn't reading at that moment, no fight.
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    let _ = tokio::io::AsyncBufReadExt::read_line(&mut stdin, &mut line).await;
    let answer = line.trim().to_lowercase();
    let approved = matches!(
        answer.as_str(),
        "y" | "yes" | "s" | "session" | "a" | "always"
    );
    let remember = match answer.as_str() {
        "s" | "session" => joyczl_protocol::RememberScope::Session,
        "a" | "always" => joyczl_protocol::RememberScope::Always,
        _ => joyczl_protocol::RememberScope::Once,
    };

    let accepted = server.answer_approval(&ask.turn_id, &ask.request_id, approved, remember);
    if !accepted {
        println!("…that question has expired (the turn is no longer waiting)");
    } else if approved {
        println!(
            "…allowed{}",
            match remember {
                joyczl_protocol::RememberScope::Once => "",
                joyczl_protocol::RememberScope::Session => ", and this session will not ask again",
                joyczl_protocol::RememberScope::Always => ", and written to settings.json",
            }
        );
    } else {
        println!("…denied");
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
                    "…memory gate: {} ({})",
                    match decided.decision.decision {
                        joyczl_protocol::GateDecisionKind::Retrieve => "retrieve",
                        joyczl_protocol::GateDecisionKind::Skip => "skip",
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
                        "…goal not met yet (round {}/{}): {}",
                        round.round, round.max_rounds, round.reason
                    ),
                    "satisfied" => println!("…goal met (round {}): {}", round.round, round.reason),
                    "impossible" => {
                        println!(
                            "…goal cannot be done (round {}): {}",
                            round.round, round.reason
                        )
                    }
                    "round-limit" => println!(
                        "…hit the round limit ({}), stopping here: {}",
                        round.max_rounds, round.reason
                    ),
                    other => println!("…goal loop stopped ({other}): {}", round.reason),
                }
            }
            // Retries are **never silent**: rather than letting the user guess at a
            // stuck interface, say plainly "rate-limited, wait a bit and try again".
            ServerNotification::Retry(retry) => {
                end_streamed_line(&mut streamed);
                println!(
                    "…the model {}; retrying in {}ms (attempt {})",
                    retry.reason, retry.delay_ms, retry.attempt
                );
            }
            // Live tool output is for a GUI that can repaint a row; a line-oriented REPL
            // would interleave it with everything else, so it waits for the completion line.
            ServerNotification::ToolDelta(_) => {}
            ServerNotification::ToolStarted(started) => {
                end_streamed_line(&mut streamed);
                println!("…calling {}…", started.tool);
            }
            ServerNotification::ToolCompleted(done) => {
                let status = if done.status == joyczl_protocol::ToolStatus::Ok {
                    "ok"
                } else {
                    "error"
                };
                println!(
                    "…{} done ({}, {}ms)",
                    done.tool,
                    status,
                    done.duration_ms.unwrap_or(0)
                );
            }
            ServerNotification::ConsolidationCompleted(done) => {
                println!("…distilled {} new facts", done.new_facts);
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
                    "— {} · {} turns · {}ms{}{}",
                    completed.meta.model,
                    completed.iterations,
                    completed.meta.latency_ms,
                    if usage.is_empty() { "" } else { " · " },
                    usage,
                );
                if completed.meta.interrupted {
                    println!("(this turn was interrupted.)");
                }
                // Guard hits are not normal: the user should know part of this turn's
                // budget went to spinning in place (which also explains the longer time).
                if completed.meta.guard_hits > 0 {
                    println!(
                        "…loop guard tripped {} times (the model kept repeating itself)",
                        completed.meta.guard_hits
                    );
                }
                break;
            }
            // Inbound mail: the terminal says how many arrived, not their content.
            ServerNotification::InboxReceived(inbox) => {
                end_streamed_line(&mut streamed);
                println!(
                    "…{} new message(s) in the inbox (connector {}); how they are handled is inbound.mode in connectors.json",
                    inbox.added, inbox.connector
                );
            }
            // A long task changed state: the terminal says which way it went, never silently.
            ServerNotification::TaskUpdated(updated) => {
                end_streamed_line(&mut streamed);
                println!(
                    "…task \"{}\": {} ({} turns){}",
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
                    "…resuming from the interruption ({} turns done; finished tools do not run again)",
                    resumed.from_iterations
                );
            }
            // Another window (or the dashboard) added a sentence to this turn. The terminal
            // cannot type mid-turn itself, but it must not look like the reply ignored it.
            ServerNotification::TurnSteered(steered) => {
                end_streamed_line(&mut streamed);
                println!("…added a line (effective next turn): {}", steered.message);
            }
            ServerNotification::TurnSuspended(suspended) => {
                end_streamed_line(&mut streamed);
                println!(
                    "…this turn is paused ({}); it will not continue by itself",
                    suspended.reason
                );
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
    if let Err(error) = task.await.expect("a turn task must not panic") {
        println!("Something went wrong: {}", error.message);
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
/// `/compact [focus]` — fold the older turns in **now**.
///
/// The same body the dashboard's button uses (`memory_center::compact_session`): a second
/// implementation would drift, and "compacted here but not there" is exactly the kind of
/// difference nobody reports until a long session is already broken.
async fn compact(server: &Server, session_id: &str, input: &str) {
    let focus = input.trim_start_matches("/compact").trim();
    match joyczl_app_server::memory_center::compact_session(
        server,
        session_id,
        (!focus.is_empty()).then_some(focus),
    )
    .await
    {
        Ok(answer) if answer.covered_turns == 0 => {
            println!("Nothing older to fold — the summary is caught up.")
        }
        Ok(answer) => println!(
            "Compacted: the summary now covers {} turns (about {} tokens).",
            answer.covered_turns, answer.summary_tokens
        ),
        Err(error) => println!("Compaction failed: {}", error.message),
    }
}

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
            "Memory{} is still empty. Tell Joy something about yourself and it will remember (state.db lives in the home directory).",
            if query.is_empty() {
                String::new()
            } else {
                format!(" about “{query}”")
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
        println!("No past sessions yet.");
        return;
    }
    for s in sessions {
        println!(
            "- {}  {} ({} messages, latest {})",
            s.id,
            s.title,
            s.messages,
            s.last_at.as_deref().unwrap_or("?")
        );
    }
}
