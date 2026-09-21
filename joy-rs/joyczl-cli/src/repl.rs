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
其他任何输入都会作为消息发给 Joy。";

/// 进入终端对话。拿到的是已装配好的 Server。
pub async fn run(server: Server) -> Result<()> {
    println!("Joy — 本地优先的个人助手。/help 看命令，/quit 退出。");
    println!();

    let mut session_id = new_session_id();
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
            s if s.starts_with('/') => {
                println!("不认识的命令 {s}。/help 看有哪些。");
            }
            message => chat_turn(&server, &session_id, message).await,
        }
    }
    println!("再见。");
    Ok(())
}

fn new_session_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("cli-{millis}")
}

/// 一轮对话：流式渲染文本，门与工具的动静各占一行小字。
async fn chat_turn(server: &Server, session_id: &str, message: &str) {
    let (sink, mut rx) = EventSink::channel();
    let params = TurnStartParams {
        session_id: Some(session_id.to_string()),
        message: message.to_string(),
        stream: Some(true),
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
