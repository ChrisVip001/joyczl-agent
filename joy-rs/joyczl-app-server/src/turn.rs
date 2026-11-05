//! Assembly of one turn — the "Ephemeral Agent Run" box on the whiteboard.
//!
//! Everything here is **rebuilt per turn and thrown away after use**: system prompt,
//! working memory, retrieved context. What persists lives in state.db (joyczl-state).
//!
//! Flow: retrieval gate → system prompt → sliding-window history → THE LOOP → persist → consolidation.
//!
//! At the very front there is also a **front door**: the triage graph (`graph_workflows`, off by default).
//! When enabled it decides whether this turn is a "quick answer" (small model replies directly, one call)
//! or "full" (the whole path below). A broken graph falls back to the full path — it can only save time, never cut capability.
//!
//! Notifications are **sent as they happen**: model text deltas are pushed to the client as soon as they
//! arrive (typewriter effect on the dashboard), and a finished tool is pushed the moment it completes.
//! The reply to `turn/start` is sent by this function itself — it must come only after all notifications are out; only it knows the order.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use chrono::Local;
use joyczl_graph::workflows::triage::{
    build_triage_graph, classify_message, todays_events, CalendarFn, ClassifyFn, FullFn, QuickFn,
    QUICK_REPLY_PROMPT,
};
use joyczl_graph::{
    run_graph, GraphEvent, NodeCtx, NodeWrites, Observer, State, DEFAULT_MAX_STEPS,
};
use joyczl_loop::{LoopEvent, LoopResult};
use joyczl_protocol::{
    codes, ConsolidationCompletedNotification, ErrorObject, GateDecidedNotification, GateDecision,
    GateDecisionKind, GoalRoundNotification, GraphEndedNotification, GraphInfo,
    GraphNodeEndedNotification, GraphNodeStartedNotification, GraphRouteKind,
    GraphStartedNotification, JsonRpcMessage, JsonRpcResponse, RequestId, RetryNotification,
    ServerNotification, TextDeltaNotification, TokenUsage, ToolCallRecord,
    ToolCompletedNotification, ToolStartedNotification, ToolStatus, TurnCheckpointedNotification,
    TurnCompletedNotification, TurnMeta, TurnStartParams, TurnStartResponse,
    TurnStartedNotification, TurnSuspendedNotification, JSONRPC_VERSION,
};
use joyczl_provider::{ContentBlock, CreateRequest, Message, Resolved, Role, TextSink, Usage};

use crate::{narrow, trace, EventSink, Server};

const DEFAULT_SOUL: &str = "\
你是 Joy，一个跑在用户自己电脑上的本地个人助手。
你简洁、温暖、主动。用户告诉你的事你会记住。

规则：
- 想安排日程、解释「今天」「明天」之前，先用 current_time 确认现在几点。
- 用户说了值得长期记住的事（关于人、项目、偏好），用 save_note 存起来。
- 用户问起过去的事、某个人，用 search_memory 查；想知道你都记了什么，用 list_memory。
- 用户明确说「忘掉…」，用 forget_note。
- 用户教你一套想复用的工作流程时，先征得同意，再用 create_skill 写成 SKILL.md。
- 下面的「相关记忆」来自你自己的记忆库，可以信任。
- 每个工具的输出都写明了结果落在哪儿 —— 如实转述，别声称同步到了别处。
- [tools used: ...] 是你过去几轮实际做过的事；已经做过的不要重复做。\
";

/// SOUL.md is the user-editable persona file, created on first run.
/// Editing it changes who Joy is — the plainest form of procedural memory.
pub(crate) fn load_soul(home: &std::path::Path) -> String {
    let path = home.join("SOUL.md");
    if !path.exists() {
        let _ = std::fs::write(&path, DEFAULT_SOUL);
    }
    std::fs::read_to_string(&path).unwrap_or_else(|_| DEFAULT_SOUL.to_string())
}

/// Media types we accept for attachments, by file extension — the extension wins over the
/// client's hint (a renamed file is the user's problem; a wrong content-type is worse).
/// Only images ride as content blocks: other files keep the "path mentioned in text" route,
/// where the model can open them with read_file.
fn media_type_for_extension(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// The hard cap per image (raw bytes). Mirrors the upload endpoint's 20 MiB; base64
/// inflates by 4/3, so this is also the honest ceiling for what one request can carry.
const IMAGE_MAX_BYTES: usize = 20 * 1024 * 1024;

/// Turn staged uploads into image content blocks. Everything is checked up front —
/// containment, existence, media type, size — and any refusal fails the turn before it
/// starts, with `Error:`-quality guidance (the same honesty rules as the tools).
fn resolve_attachments(
    server: &Server,
    params: &TurnStartParams,
    provider_id: &str,
    vision: bool,
) -> Result<Vec<joyczl_provider::ContentBlock>, ErrorObject> {
    let Some(attachments) = &params.attachments else {
        return Ok(Vec::new());
    };
    if attachments.is_empty() {
        return Ok(Vec::new());
    }
    let bad = |message: String| ErrorObject {
        code: codes::INVALID_PARAMS,
        message,
        data: None,
    };
    if !vision {
        return Err(bad(format!(
            "当前厂商 {provider_id} 的默认模型不支持图片输入。换支持视觉的厂商 \
             （anthropic / openai / gemini / xai），或在 <home>/models-catalog.json 给 \
             对应行加 \"vision\": true 后重启。"
        )));
    }
    let home = server.settings().home;
    let home_real = home
        .canonicalize()
        .map_err(|e| bad(format!("home 目录不可用：{e}")))?;
    let mut images = Vec::new();
    for attachment in attachments {
        let raw = home.join(&attachment.path);
        // Symlinks resolve first: the real path must stay inside home, no exceptions.
        let path = raw
            .canonicalize()
            .map_err(|_| bad(format!("附件不存在或不可读：{}", attachment.path)))?;
        if !path.starts_with(&home_real) {
            return Err(bad(format!(
                "附件 {} 在 home 之外 —— 附件只能引用上传目录（<home>/uploads/…）里的文件。",
                attachment.path
            )));
        }
        let media_type = path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(media_type_for_extension)
            .map(str::to_string)
            .or_else(|| {
                attachment
                    .media_type
                    .as_deref()
                    .filter(|m| m.starts_with("image/"))
                    .map(str::to_string)
            })
            .ok_or_else(|| {
                bad(format!(
                    "附件 {} 不是可识别的图片类型（支持 png / jpg / jpeg / gif / webp）；\
                     其它文件请直接在正文里写路径，模型会用 read_file 打开。",
                    attachment.path
                ))
            })?;
        let bytes = std::fs::read(&path).map_err(|e| bad(format!("读附件失败：{e}")))?;
        if bytes.len() > IMAGE_MAX_BYTES {
            return Err(bad(format!(
                "附件 {} 有 {} 字节，超过单张图片上限 {IMAGE_MAX_BYTES}。",
                attachment.path,
                bytes.len()
            )));
        }
        use base64::Engine as _;
        images.push(joyczl_provider::ContentBlock::Image {
            media_type: media_type.to_string(),
            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
        });
    }
    Ok(images)
}

pub async fn run_turn(
    server: &Server,
    params: TurnStartParams,
    id: RequestId,
    sink: &EventSink,
) -> Result<(), ErrorObject> {
    let started = std::time::Instant::now();

    // Model not configured: spell out "what to do" instead of just dropping a "no key".
    let resolved: Resolved = server.resolved().ok_or_else(|| ErrorObject {
        code: codes::PROVIDER_ERROR,
        message: "模型还没配好：缺 API key。配一套就能用 —— \
                  /model add <名字> <厂商> --env 变量名（key 交给系统钥匙串），\
                  或在驾驶舱的「模型」面板里填。"
            .to_string(),
        data: None,
    })?;

    // Auto-select a model by task tier (route.rs). In fixed mode, or when the selected model is
    // already active, return as-is with zero overhead; the fallback and selection reasons go into route_reason, unhidden.
    let (resolved, turn_route) = server.resolved_for_turn(resolved, &params.message).await;

    // ---- Attachments: staged images, read + base64'd here. Gates on the provider's vision
    // capability and the file's existence **before any side effect** — a bad attachment must
    // not leave a half-turn behind.
    let user_images = resolve_attachments(server, &params, &resolved.provider_id, resolved.vision)?;

    let session_id = params
        .session_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "default".to_string());
    let turn_id = new_turn_id();

    // ---- Long task: if the session has an active one, this turn carries it. The heartbeat
    // (`current_turn_id`) is what `task/suspend`, `task/resume` and the boot reconcile hang on.
    let task = server.tasks.active(&session_id).await.ok().flatten();
    if let Some(task) = &task {
        match server.tasks.set_running(&task.id, &turn_id).await {
            Ok(()) => {
                if let Ok(Some(row)) = server.tasks.get(&task.id).await {
                    crate::task::notify(sink, &row, None);
                }
            }
            Err(e) => eprintln!("(joy) 任务心跳没记上（任务卡片会显示旧状态）：{e}"),
        }
    }
    let task_id = task.as_ref().map(|task| task.id.clone());

    // Register the cancellation token: turn/interrupt finds it by turn_id. However this turn
    // exits (success, error, interrupted), the guard's Drop removes it from the table.
    let interrupt = server.register_turn(&turn_id);
    let _guard = TurnGuard {
        server,
        turn_id: turn_id.clone(),
    };

    // ---- The checkpoint outlet (shared with the resumed turn — see checkpoint_outlet).
    let checkpoint = checkpoint_outlet(
        &server.checkpoints,
        sink,
        &turn_id,
        &session_id,
        &params.message,
    );

    // Working directory for this turn: validate when provided (fail-closed). Reject before the
    // turn starts, leaving no half-turn behind.
    let turn_cwd = resolve_turn_cwd(&params.cwd)?;
    // Only actually-used directories enter "recently used" (workspaces.json) — quick entries for the cockpit picker.
    if let Some(path) = &turn_cwd {
        joyczl_config::workspaces::record_workspace(
            &server.settings().home,
            &path.display().to_string(),
        );
    }

    sink.notification(ServerNotification::TurnStarted(TurnStartedNotification {
        turn_id: turn_id.clone(),
        session_id: session_id.clone(),
        user_message: params.message.clone(),
        ts: Local::now().to_rfc3339(),
    }));

    // ---- SessionStart: the first turn of this session. The returned `additionalContext` merges into this
    // turn's message (the hook may want to lay some background for the model, e.g. "this repo's rules live in CONTRIBUTING.md").
    let mut message = params.message.clone();
    if server
        .chat
        .session_history(&session_id)
        .await
        .unwrap_or_default()
        .is_empty()
    {
        let payload = serde_json::json!({
            "session_id": session_id,
            "cwd": std::env::current_dir().ok(),
            "user_message": params.message,
        });
        let outcome = crate::fire_hook(
            &server.hooks,
            joyczl_tools::hooks::HookEvent::SessionStart,
            payload,
        )
        .await;
        if let Some(context) = outcome.context {
            // Say explicitly that this came from a hook: the model must not assume the user said it.
            message = format!("{context}\n\n{message}");
        }
        if let Some(reason) = outcome.blocked {
            return Err(joyczl_protocol::ErrorObject {
                code: joyczl_protocol::codes::INTERNAL_ERROR,
                message: format!("SessionStart hook 拒绝了这次会话：{reason}"),
                data: None,
            });
        }
    }

    // ---- Retry notifications: **sent on every occurrence**.
    //
    // The counter flows through here too — `meta.retries` is therefore a measured value, not a
    // guess. What the user should see is "rate limited, retrying", not an inexplicably long pause.
    let retries = Arc::new(AtomicI32::new(0));
    let retry_sink: joyczl_provider::retry::NoteSink = {
        let counter = retries.clone();
        let sink = sink.clone();
        let turn_id = turn_id.clone();
        Arc::new(move |notice: joyczl_provider::retry::RetryNotice| {
            counter.fetch_add(1, Ordering::Relaxed);
            sink.notification(ServerNotification::Retry(RetryNotification {
                turn_id: turn_id.clone(),
                attempt: narrow(notice.attempt as i64),
                reason: notice.reason.clone(),
                delay_ms: narrow(notice.delay_ms as i64),
            }));
        })
    };

    // ---- Front door: the triage graph (off by default).
    //
    // The graph can only make this turn **faster**, never worse: any failure at any step falls back to
    // the ordinary full loop below — the same "fail-open" rule as the retrieval gate.
    //
    // The whole section (graph and full_turn) is wrapped in the notification outlet: model calls may
    // happen anywhere along either path, so retry notifications must not cover only one of them.
    // Annotate the error type explicitly: no more `?` bubbling up here; the compiler needs to know what it is.
    let dispatched: Result<_, joyczl_protocol::ErrorObject> =
        joyczl_provider::retry::with_note_sink(retry_sink, async {
            match graph_route(
                server,
                &resolved,
                &message,
                TurnFrame {
                    session_id: &session_id,
                    turn_id: &turn_id,
                    sink,
                    interrupt: Some(interrupt.clone()),
                    turn_cwd: turn_cwd.clone(),
                    route_reason: turn_route.clone(),
                    resume: None,
                    checkpoint: Some(checkpoint.clone()),
                },
            )
            .await?
            {
                Some(routed) => Ok((Some(routed.info), routed.turn)),
                None => Ok((
                    None,
                    full_turn(
                        server,
                        &resolved,
                        &message,
                        user_images.clone(),
                        TurnFrame {
                            session_id: &session_id,
                            turn_id: &turn_id,
                            sink,
                            interrupt: Some(interrupt.clone()),
                            turn_cwd: turn_cwd.clone(),
                            route_reason: turn_route.clone(),
                            resume: None,
                            checkpoint: Some(checkpoint.clone()),
                        },
                        None,
                    )
                    .await?,
                )),
            }
        })
        .await;

    let (graph, mut full) = match dispatched {
        Ok(pair) => pair,
        Err(e) => {
            // This turn **did not finish** (model died, graph blew up...): close both ledgers
            // honestly before leaving — an open checkpoint would be resumed on next boot as if
            // the failure never happened, and the task card would keep saying "running".
            if let Err(write) = server.checkpoints.finish(&turn_id, "abandoned").await {
                eprintln!("(joy) 失败回合的 checkpoint 没标上：{write}");
            }
            if let Some(id) = &task_id {
                let _ = server.tasks.finish(id, "failed").await;
                if let Ok(Some(row)) = server.tasks.get(id).await {
                    crate::task::notify(sink, &row, Some(&e.message));
                }
            }
            // StopFailure: this turn **did not finish** (model died, graph blew up...). Observe-only event —
            // problems in the hook itself must not overwrite the original error.
            crate::fire_hook(
                &server.hooks,
                joyczl_tools::hooks::HookEvent::StopFailure,
                serde_json::json!({
                    "session_id": session_id,
                    "turn_id": turn_id,
                    "error": e.message,
                }),
            )
            .await;
            return Err(e);
        }
    };

    // ---- Stop: this turn finished. If the hook demands continuation (exit 2 / decision: block), run one
    // more turn, capped at 1. The generic "auto-continue" belongs to the goal loop (`goal/set`); here we
    // only honor "Stop can hold off the end", without building a second loop out of it.
    let mut stop_rounds = 0usize;
    loop {
        let payload = serde_json::json!({
            "session_id": session_id,
            "turn_id": turn_id,
            "reply": full.result.reply,
            "iterations": full.result.iterations,
        });
        let outcome =
            crate::fire_hook(&server.hooks, joyczl_tools::hooks::HookEvent::Stop, payload).await;
        let Some(reason) = outcome.blocked else {
            break;
        };
        if stop_rounds >= MAX_STOP_CONTINUATIONS {
            // Continuation demanded even after continuing: say so honestly, don't pretend it is satisfied.
            full.result
                .reply
                .push_str(&format!("\n\n（Stop hook 仍要求继续：{reason}）"));
            break;
        }
        stop_rounds += 1;
        eprintln!("(joy) Stop hook 要求继续（第 {stop_rounds} 次）：{reason}");
        full = full_turn(
            server,
            &resolved,
            &reason,
            Vec::new(),
            TurnFrame {
                session_id: &session_id,
                turn_id: &turn_id,
                sink,
                interrupt: Some(interrupt.clone()),
                turn_cwd: turn_cwd.clone(),
                route_reason: turn_route.clone(),
                resume: None,
                checkpoint: Some(checkpoint.clone()),
            },
            None,
        )
        .await?;
    }

    // ---- Goal loop: the main loop stopping **does not mean the goal is met**.
    //
    // Only a human can set a goal (`goal/set`; the model does not even have that tool), so continuing here
    // is not "the model assigning itself work" — it is the human having said earlier "keep going until X".
    let mut goal_status: Option<String> = None;
    let mut goal_rounds = 0i32;
    if let Some(goal) = crate::goal::active(&server.goals, &session_id) {
        let max_rounds = server
            .settings
            .read()
            .expect("settings 锁不该中毒")
            .goal_max_rounds;
        eprintln!(
            "(joy) 目标循环开始：{condition}",
            condition = goal.condition
        );
        loop {
            goal_rounds = crate::goal::bump(&server.goals, &session_id);
            // The round account lives on the task row too: the card shows how much was spent
            // even after a restart (the in-memory mirror alone would forget).
            if let Some(id) = &task_id {
                if let Err(e) = server.tasks.record_round(id, goal_rounds).await {
                    eprintln!("(joy) 轮次没记进任务台账：{e}");
                }
            }
            let transcript = judge_transcript(server, &session_id, &full.result.reply).await;
            let verdict = crate::goal::judge(&resolved, &goal.condition, &transcript).await;

            let judgement = match verdict {
                Ok(judgement) => judgement,
                Err(why) => {
                    // The judge broke down: stop continuing, **keep the goal** (don't clear it, don't fake success).
                    notify_goal(sink, &turn_id, goal_rounds, max_rounds, "blocked", &why);
                    eprintln!("(joy) 目标循环停了：{why}");
                    goal_status = Some("blocked".to_string());
                    crate::goal::finish(&server.goals, &session_id, "blocked");
                    server.notify_goal_finished(&session_id, "blocked", &why);
                    break;
                }
            };

            if judgement.ok {
                notify_goal(
                    sink,
                    &turn_id,
                    goal_rounds,
                    max_rounds,
                    "satisfied",
                    &judgement.reason,
                );
                goal_status = Some("satisfied".to_string());
                crate::goal::finish(&server.goals, &session_id, "satisfied");
                server.notify_goal_finished(&session_id, "satisfied", &judgement.reason);
                break;
            }
            if judgement.impossible {
                notify_goal(
                    sink,
                    &turn_id,
                    goal_rounds,
                    max_rounds,
                    "impossible",
                    &judgement.reason,
                );
                goal_status = Some("impossible".to_string());
                crate::goal::finish(&server.goals, &session_id, "impossible");
                server.notify_goal_finished(&session_id, "impossible", &judgement.reason);
                break;
            }
            if goal_rounds >= max_rounds {
                // Cap reached: **say honestly it is not done** — don't fake completion, don't clear the goal.
                notify_goal(
                    sink,
                    &turn_id,
                    goal_rounds,
                    max_rounds,
                    "round-limit",
                    &format!("续了 {max_rounds} 轮还没达成：{}", judgement.reason),
                );
                goal_status = Some("round-limit".to_string());
                crate::goal::finish(&server.goals, &session_id, "round-limit");
                server.notify_goal_finished(&session_id, "round-limit", &judgement.reason);
                break;
            }

            notify_goal(
                sink,
                &turn_id,
                goal_rounds,
                max_rounds,
                "continuing",
                &judgement.reason,
            );
            eprintln!(
                "(joy) 目标循环第 {goal_rounds} 轮（上限 {max_rounds}）：{}",
                judgement.reason
            );
            // Feed the reason back as the next user message and run another round **in the same session**.
            full = full_turn(
                server,
                &resolved,
                &judgement.reason,
                Vec::new(),
                TurnFrame {
                    session_id: &session_id,
                    turn_id: &turn_id,
                    sink,
                    interrupt: Some(interrupt.clone()),
                    turn_cwd: turn_cwd.clone(),
                    route_reason: turn_route.clone(),
                    resume: None,
                    checkpoint: Some(checkpoint.clone()),
                },
                None,
            )
            .await?;
        }
    }

    let FullTurn { result, gate } = full;

    finalize_turn(
        server,
        sink,
        FinishedTurn {
            session_id,
            turn_id: turn_id.clone(),
            user_message: &params.message,
            resolved: &resolved,
            graph,
            gate,
            result,
            route_reason: turn_route.clone(),
            retries: retries.load(Ordering::Relaxed),
            goal_status,
            goal_rounds,
            started,
        },
    )
    .await?;

    // Only after every notification is out does the response get its turn.
    sink.response(JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id,
        result: serde_json::to_value(TurnStartResponse { turn_id }).expect("序列化"),
    }));
    Ok(())
}

/// Everything one finished turn leaves behind — shared by the fresh path and the resumed one,
/// so "a resumed turn gets exactly the same accounting" holds by construction, not by copy-paste.
pub(crate) struct FinishedTurn<'a> {
    pub(crate) session_id: String,
    pub(crate) turn_id: String,
    pub(crate) user_message: &'a str,
    pub(crate) resolved: &'a Resolved,
    pub(crate) graph: Option<GraphInfo>,
    pub(crate) gate: Option<GateDecision>,
    pub(crate) result: LoopResult,
    pub(crate) route_reason: Option<String>,
    pub(crate) retries: i32,
    pub(crate) goal_status: Option<String>,
    pub(crate) goal_rounds: i32,
    pub(crate) started: std::time::Instant,
}

/// The shared tail of every turn: notifications, the checkpoint ledger's closing entry, the
/// trace, the usage ledger, the write section (history append → consolidation → MEMORY.md) and
/// the `TurnCompleted` announcement. The reply goes to the response by `run_turn` itself — only
/// it knows the order, and a resumed turn has no response to send at all.
pub(crate) async fn finalize_turn(
    server: &Server,
    sink: &EventSink,
    t: FinishedTurn<'_>,
) -> Result<(), ErrorObject> {
    let FinishedTurn {
        session_id,
        turn_id,
        user_message,
        resolved,
        graph,
        gate,
        result,
        route_reason,
        retries,
        goal_status,
        goal_rounds,
        started,
    } = t;
    let settings = server.settings();

    // ---- Checkpoint bookkeeping: the turn reached an end **in this process**. From here on,
    // `open` can only mean "the process died mid-turn" — the invariant the startup reconcile
    // relies on. Two kinds of interruption must not be conflated:
    //   * a stop (turn/interrupt): `abandoned`, never auto-resumed;
    //   * a pause (task/suspend): `suspended`, kept resumable — `task/resume` continues it.
    let paused = server.take_suspending(&turn_id);
    let (final_status, suspended_reason) = if paused {
        ("suspended", Some("已暂停，可从断点继续"))
    } else if result.interrupted {
        ("abandoned", Some("interrupted by user"))
    } else {
        ("done", None)
    };
    if let Err(e) = server.checkpoints.finish(&turn_id, final_status).await {
        eprintln!("(joy) checkpoint 收尾状态没记上（不影响回合本身）：{e}");
    }
    if let Some(reason) = suspended_reason {
        sink.notification(ServerNotification::TurnSuspended(
            TurnSuspendedNotification {
                turn_id: turn_id.clone(),
                session_id: session_id.clone(),
                reason: reason.to_string(),
            },
        ));
    }

    // ---- Task bookkeeping: the same single tail (a resumed turn goes through here too).
    if let Some(task) = server.tasks.by_turn(&turn_id).await.ok().flatten() {
        let verdict_reason = match (paused, result.interrupted, goal_status.as_deref()) {
            // Paused or stopped by the human: the task waits, it did not finish.
            (true, _, _) | (_, true, _) => {
                let reason = if paused {
                    "已暂停，可从断点继续"
                } else {
                    "用户中断，任务停在原地"
                };
                let _ = server.tasks.suspend(&task.id, Some(reason)).await;
                Some(reason.to_string())
            }
            // The loop's own verdicts, mapped honestly: not-done is not done.
            (_, _, Some("satisfied")) => {
                let _ = server.tasks.finish(&task.id, "done").await;
                None
            }
            (_, _, Some("impossible")) | (_, _, Some("round-limit")) => {
                let _ = server.tasks.finish(&task.id, "failed").await;
                goal_status.clone()
            }
            // The judge broke down: keep the task alive (suspended) — the human decides.
            (_, _, Some("blocked")) => {
                let _ = server
                    .tasks
                    .suspend(&task.id, Some("判断器故障，已停在这里"))
                    .await;
                Some("判断器故障，已停在这里".to_string())
            }
            // No goal condition = single-turn task; or a goal that simply did not run this turn.
            (_, _, None) => {
                if task.goal_condition.is_some() {
                    let _ = server
                        .tasks
                        .suspend(&task.id, Some("目标这一轮没推进，等待下一句话"))
                        .await;
                    Some("目标这一轮没推进，等待下一句话".to_string())
                } else {
                    let _ = server.tasks.finish(&task.id, "done").await;
                    None
                }
            }
            (_, _, Some(_)) => {
                let _ = server.tasks.clear_turn(&task.id).await;
                None
            }
        };
        if let Ok(Some(row)) = server.tasks.get(&task.id).await {
            crate::task::notify(sink, &row, verdict_reason.as_deref());
        }
    }

    for call in &result.tool_calls {
        sink.notification(ServerNotification::ToolCompleted(
            ToolCompletedNotification {
                turn_id: turn_id.clone(),
                tool: call.name.clone(),
                output: call.output.clone(),
                status: if call.ok() {
                    ToolStatus::Ok
                } else {
                    ToolStatus::Error
                },
                duration_ms: Some(narrow(call.duration_ms)),
            },
        ));
    }

    // Tool activity is folded into the assistant text. Without this, the model forgets what it already
    // did and does it again next turn (that is how triple-booked meetings happen).
    let reply = fold_tool_activity(&result.reply, &result.tool_calls);

    // The quick answer came from the small model. Record that honestly — so "what model are you" won't be answered wrong by the model itself.
    let quick = graph
        .as_ref()
        .is_some_and(|info| info.route == GraphRouteKind::Quick);
    let meta = TurnMeta {
        gate,
        graph,
        iterations: result.iterations,
        latency_ms: started.elapsed().as_millis() as i32,
        tools: result
            .tool_calls
            .iter()
            .map(|c| ToolCallRecord {
                tool: c.name.clone(),
                status: if c.ok() {
                    ToolStatus::Ok
                } else {
                    ToolStatus::Error
                },
                duration_ms: Some(narrow(c.duration_ms)),
            })
            .collect(),
        model: if quick {
            resolved.small_model.clone()
        } else {
            resolved.model.clone()
        },
        provider: resolved.provider_id.clone(),
        usage: Some(TokenUsage {
            input_tokens: narrow(result.usage.input_tokens),
            output_tokens: narrow(result.usage.output_tokens),
        }),
        interrupted: result.interrupted,
        guard_hits: result.guard.hits,
        guard_note: result.guard.note.clone(),
        retries,
        goal_status: goal_status.clone(),
        goal_rounds,
        route_reason: route_reason.clone(),
    };

    let meta_json = serde_json::to_string(&meta).ok();

    // ---- Persist: one trace line + one usage line. Observability is not optional, but it must not drag
    // the conversation down either — write failures inside the two functions only shout to stderr.
    trace::record_turn(
        &settings.home,
        &serde_json::json!({
            "ts": Local::now().to_rfc3339(),
            "turnId": turn_id,
            "sessionId": session_id,
            "userMessage": user_message,
            "reply": reply,
            "meta": meta,
        }),
    );
    trace::record_usage(
        &settings.home,
        &serde_json::json!({
            "ts": Local::now().to_rfc3339(),
            "turnId": turn_id,
            "sessionId": session_id,
            "provider": meta.provider,
            "model": meta.model,
            "inputTokens": result.usage.input_tokens,
            "outputTokens": result.usage.output_tokens,
            "iterations": result.iterations,
            "interrupted": result.interrupted,
        }),
    );

    // Usage ledger (migration 0010): bookkeeping only, never enforced. A write failure does not block
    // the turn — bookkeeping is a side path, not the main road; a note on stderr is enough.
    if let Err(e) = server
        .usage
        .record(
            &session_id,
            &meta.provider,
            &meta.model,
            result.usage.input_tokens,
            result.usage.output_tokens,
        )
        .await
    {
        eprintln!("(joy) 用量没记进台账：{e}");
    }

    // ---- Write section: per-session serialisation. From the history append to the MEMORY.md
    // mirror, this session's writes must not interleave with another concurrent turn's write
    // section — that is the lock's incremental value: the three SQLite guards stop double-write
    // corruption, this stops read/write interleaving. Model calls never hold it.
    let session_write_lock = server.session_write_lock(&session_id);
    let write_guard = session_write_lock.lock().await;

    server
        .chat
        .append_exchange(
            user_message,
            &reply,
            &session_id,
            "app-server",
            meta_json.as_deref(),
        )
        .await
        .map_err(|e| ErrorObject {
            code: codes::INTERNAL_ERROR,
            message: format!("写对话日志失败：{e}"),
            data: None,
        })?;

    // ---- Calibrate estimates with measurements: save this turn's last request's (measured prefill, local estimate);
    // next turn's budget math corrects by the ratio. Failure only shouts — calibration is an optimization, not this turn's ledger.
    if let Err(e) = server
        .chat
        .observe_context(
            &session_id,
            result.observed_input_tokens as i64,
            result.estimated_input_tokens as i64,
        )
        .await
    {
        eprintln!("(joy) 记上下文实测值失败（不影响这一轮）：{e}");
    }

    // ---- Consolidate when enough has accumulated. Failure never loses data — consolidation guarantees that internally.
    let new_facts = joyczl_memory::consolidation::consolidate_if_due(
        &server.chat,
        &server.facts,
        &server.episodes,
        resolved.client.as_ref(),
        &resolved.small_model,
        settings.consolidate_every,
    )
    .await
    .unwrap_or(0);
    if new_facts > 0 {
        sink.notification(ServerNotification::ConsolidationCompleted(
            ConsolidationCompletedNotification {
                new_facts: new_facts as i32,
            },
        ));
    }

    // ---- MEMORY.md mirror: refresh a human-readable view every turn.
    // It is a generated view, not the source of truth; a write failure only warns — a mirror must not
    // make the whole turn error out.
    if let Err(e) =
        joyczl_memory::export_markdown(&server.facts, &server.episodes, &settings.home).await
    {
        eprintln!("(joy) MEMORY.md 镜像失败（记忆本身不受影响）：{e}");
    }
    // Write section ends here: notifications and the response are not writes — don't let the
    // lock linger a step longer.
    drop(write_guard);

    sink.notification(ServerNotification::TurnCompleted(Box::new(
        TurnCompletedNotification {
            turn_id,
            reply: result.reply.clone(),
            iterations: result.iterations,
            usage: Some(TokenUsage {
                input_tokens: narrow(result.usage.input_tokens),
                output_tokens: narrow(result.usage.output_tokens),
            }),
            meta,
        },
    )));
    Ok(())
}

/// The output of one full loop, plus this turn's retrieval-gate decision.
///
/// The gate is a detail **inside the full path**: the quick-answer path does not even consult memory —
/// that is exactly why it is cheap, and why it has no gate.
pub(crate) struct FullTurn {
    pub(crate) result: LoopResult,
    pub(crate) gate: Option<GateDecision>,
}

/// The checkpoint outlet, constructed once per turn (fresh or resumed): every tool-results
/// boundary lands in state.db **before** the next model call goes out. A crash then costs
/// at most "finish this iteration again", never the whole turn. A save failure warns and
/// moves on — checkpointing must not stall the turn.
pub(crate) fn checkpoint_outlet(
    checkpoints: &joyczl_state::Checkpoints,
    sink: &EventSink,
    turn_id: &str,
    session_id: &str,
    user_message: &str,
) -> joyczl_loop::CheckpointSink {
    let (checkpoints, session, user_message) = (
        checkpoints.clone(),
        session_id.to_string(),
        user_message.to_string(),
    );
    let (sink, turn_id) = (sink.clone(), turn_id.to_string());
    Arc::new(move |cp: joyczl_loop::Checkpoint| {
        let (checkpoints, session, user_message) =
            (checkpoints.clone(), session.clone(), user_message.clone());
        let (sink, turn_id) = (sink.clone(), turn_id.clone());
        Box::pin(async move {
            let messages_json = serde_json::to_string(&cp.messages).unwrap_or_default();
            let usage_json = serde_json::to_string(&cp.usage).unwrap_or_default();
            if let Err(e) = checkpoints
                .save(
                    &turn_id,
                    &session,
                    &user_message,
                    &messages_json,
                    cp.iterations,
                    &usage_json,
                )
                .await
            {
                eprintln!("(joy) checkpoint 没写进去（照常跑，崩了得整轮重来）：{e}");
                return;
            }
            sink.notification(ServerNotification::TurnCheckpointed(
                TurnCheckpointedNotification {
                    turn_id,
                    session_id: session,
                    iterations: cp.iterations,
                },
            ));
        })
    })
}

/// Deregistration guard for a turn: however this turn exits (success, error, interrupted),
/// the cancellation token is removed from the table — a finished turn has nothing left to interrupt.
pub(crate) struct TurnGuard<'a> {
    pub(crate) server: &'a Server,
    pub(crate) turn_id: String,
}

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        self.server.finish_turn(&self.turn_id);
    }
}

/// What one trip through the graph's front door yields: how this turn ran, and the answer itself.
struct Routed {
    info: GraphInfo,
    turn: FullTurn,
}

/// One full loop: retrieval gate → system prompt → sliding-window history → THE LOOP.
///
/// When the graph's front door is open, the `full_agent` node calls this function — the default path
/// without the graph and the path inside the graph **share the same implementation**, so the
/// "loop as node" can never drift from the "loop as default".
///
/// `inner` is the event outlet the node leaves for the loop: when running inside the graph, events must
/// leave through it so the engine can attach `node=full_agent`. `None` when there is no graph.
///
/// Turns force-kept on overflow retry: keep the newest two turns verbatim, fold the rest into the summary.
const FORCED_RETRY_TURNS: usize = 2;

/// Reserve for the summary section: the summary model's cap (700 tokens) + section header and slack.
/// The summary is generated inside load_history; at budget time it does not exist yet, so it must be set aside up front.
const SUMMARY_RESERVE_TOKENS: usize = 800;

/// This turn's context budget (tokens): window (`JOY_CONTEXT_WINDOW` first, else the
/// provider table's approximation) × ratio. Floor of 1024 — however small the ratio, never squeeze
/// the window below fitting even one turn of conversation.
///
/// `factor` is last turn's measured/estimated ratio (see `session_context`): **calibrate estimates with measurements**.
/// When the estimate is too small (factor > 1), shrink the budget so compaction happens sooner; relax when it is too large.
/// No record (first turn, or the graph path) means no calibration.
fn budget_tokens(
    resolved: &Resolved,
    settings: &joyczl_config::Settings,
    factor: Option<f64>,
) -> usize {
    let window = settings
        .context_window
        .unwrap_or_else(|| resolved.context_window());
    let raw = window as f64 * settings.compact_threshold;
    let calibrated = match factor {
        Some(factor) if factor > 0.0 => raw / factor,
        _ => raw,
    };
    (calibrated as usize).max(1024)
}

/// Is this error a "context overflow"? The loop wrapped the provider error in anyhow;
/// drill down one level here to recognize it — overflow is the only error worth "retry once after compaction".
fn is_context_overflow(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<joyczl_provider::ProviderError>()
        .is_some_and(joyczl_provider::ProviderError::is_context_overflow)
}

/// Positioning info for one turn: session, turn, event outlet, cancellation token, working directory.
/// The same reason as Injections for bundling into a struct: they **always change together**,
/// and clippy stops at the 8th parameter.
pub(crate) struct TurnFrame<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) turn_id: &'a str,
    pub(crate) sink: &'a EventSink,
    pub(crate) interrupt: Option<Arc<joyczl_loop::Interrupt>>,
    pub(crate) turn_cwd: Option<std::path::PathBuf>,
    /// This turn's route decision reason (route.rs); None = no selection happened.
    pub(crate) route_reason: Option<String>,
    /// Resume from a checkpoint instead of assembling history (crash recovery).
    pub(crate) resume: Option<joyczl_loop::Resume>,
    /// The checkpoint outlet: every tool-results boundary lands here (None = no checkpointing).
    pub(crate) checkpoint: Option<joyczl_loop::CheckpointSink>,
}

/// Admission check for this turn's working directory (fail-closed): when given, it must be an existing
/// absolute path — silent fallback turns "I thought I was working in dir A" into the hardest bug to trace.
/// An empty string counts as not given.
pub(crate) fn resolve_turn_cwd(
    cwd: &Option<String>,
) -> Result<Option<std::path::PathBuf>, ErrorObject> {
    let Some(text) = cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let path = std::path::PathBuf::from(text);
    if !path.is_absolute() {
        return Err(ErrorObject {
            code: codes::INVALID_PARAMS,
            message: format!("cwd '{text}' 必须是绝对路径"),
            data: None,
        });
    }
    if !path.is_dir() {
        return Err(ErrorObject {
            code: codes::INVALID_PARAMS,
            message: format!("cwd '{text}' 不存在或不是目录"),
            data: None,
        });
    }
    Ok(Some(path))
}

pub(crate) async fn full_turn(
    server: &Server,
    resolved: &Resolved,
    message: &str,
    user_images: Vec<joyczl_provider::ContentBlock>,
    frame: TurnFrame<'_>,
    inner: Option<joyczl_loop::Observer>,
) -> Result<FullTurn, ErrorObject> {
    let TurnFrame {
        session_id,
        turn_id,
        sink,
        interrupt,
        turn_cwd,
        // route_reason is not used here: TurnMeta is assembled by run_turn (which holds turn_route).
        resume,
        checkpoint,
        ..
    } = frame;
    let settings = server.settings();

    // ---- Retrieval gate: does this turn consult memory?
    //
    // A resumed turn skips the gate: the memory context that mattered is already baked into the
    // checkpointed working memory — re-gating would spend a small-model call to re-decide a past.
    let (gate_decision, memory_context) = match &resume {
        Some(_) => (
            GateDecision {
                decision: GateDecisionKind::Skip,
                reason: "resumed from checkpoint —— 续跑不再过门".to_string(),
                query: None,
            },
            String::new(),
        ),
        None => {
            let decision = joyczl_memory::gate::should_retrieve(
                resolved.client.as_ref(),
                &resolved.small_model,
                message,
            )
            .await;
            let gate_decision = GateDecision {
                decision: if decision.retrieve {
                    GateDecisionKind::Retrieve
                } else {
                    GateDecisionKind::Skip
                },
                reason: decision.reason.clone(),
                query: decision.retrieve.then(|| decision.query.clone()),
            };
            sink.notification(ServerNotification::GateDecided(GateDecidedNotification {
                turn_id: turn_id.to_string(),
                decision: gate_decision.clone(),
            }));
            let memory_context = if decision.retrieve {
                // The hybrid-retrieval leg: the embedder is assembled only when the switch is on. Assembly failure
                // (no model configured, provider lacks a default endpoint) warns once and retrieval falls back to
                // pure keywords — one misconfigured knob must not keep the whole conversation from starting.
                let embedder = if settings.embeddings_enabled {
                    match joyczl_provider::embed::Embedder::from_settings(&settings) {
                        Ok(embedder) => Some(embedder),
                        Err(why) => {
                            eprintln!("(joy) 向量检索没配好，这次只用关键词：{why}");
                            None
                        }
                    }
                } else {
                    None
                };
                joyczl_memory::retrieve_context(
                    &server.facts,
                    &server.episodes,
                    &decision.query,
                    settings.retrieval_top_k.max(1) as u32,
                    embedder.as_ref(),
                )
                .await
                .unwrap_or_default()
            } else {
                String::new()
            };
            (gate_decision, memory_context)
        }
    };

    // ---- Procedural memory: SKILL.md progressive disclosure. Frontmatter is always scanned (cheap),
    // the body enters the prompt only when the message matches.
    let mut skill_loader = joyczl_memory::skills::SkillLoader::new(
        joyczl_memory::skills::SkillLoader::dirs_for(&settings.home),
    );
    let skill_hits = skill_loader.hits(message);
    let skills = skill_hits.section;
    // Explicit references (`$skill-name`) are stripped before sending to the model — references are for
    // the loader; the body text is enough for it. A reference to a nonexistent skill appends a hint, so it
    // can reply instead of pretending not to see. The persisted history remains **the user's original words** (run_turn's business).
    let prompt_message = if skill_hits.hints.is_empty() {
        skill_hits.message.clone()
    } else {
        format!("{}\n\n{}", skill_hits.hints.join("\n"), skill_hits.message)
    };

    // ---- Working memory: sliding window + rolling summary + token budget.
    //
    // The turn count is the **ceiling**, tokens are the gate: one long tool output can blow past the
    // window while the turn count still looks "safe". The budget is "context window × JOY_COMPACT_THRESHOLD"
    // minus the parts already committed to spend (system prefix, tool declarations, this turn's answer, the summary section).
    // Estimation is approximate (cl100k), so slack is deliberately left wide.
    let soul = load_soul(&settings.home);
    let reserve = joyczl_provider::tokens::estimate_text(&soul)
        + joyczl_provider::tokens::estimate_text(&memory_context)
        + joyczl_provider::tokens::estimate_text(&skills)
        + joyczl_provider::tokens::estimate_tools(&server.tools.schemas())
        + settings.max_tokens.max(0) as usize
        + SUMMARY_RESERVE_TOKENS;
    // Last turn's measured/estimated ratio: use it to calibrate this estimate (no record, no calibration).
    let factor = server.chat.context_factor(session_id).await.unwrap_or(None);
    let history_budget = budget_tokens(resolved, &settings, factor).saturating_sub(reserve);

    // ---- Compaction may happen here (compaction::refresh inside load_history), and it belongs
    // to this session's write section: serialised against another concurrent turn's write
    // section. The lock is held only while reading history/compacting — THE LOOP and interrupt
    // never wait on it.
    let session_write_lock = server.session_write_lock(session_id);
    // A resumed turn skips history assembly: its working memory IS the checkpoint — history,
    // user message and finished tool rounds are all already inside it. Compaction (a model
    // call) must not run just to produce a summary that gets discarded.
    let (mut history, mut summary) = if resume.is_some() {
        (Vec::new(), None)
    } else {
        {
            let _compact_guard = session_write_lock.lock().await;
            load_history(
                &server.chat,
                resolved,
                session_id,
                settings.history_turns,
                history_budget,
                None,
                &server.hooks,
            )
            .await
        }
    };
    let mut system = build_system(
        &soul,
        &resolved.model,
        &resolved.provider_id,
        &memory_context,
        &skills,
        server.todo.render(session_id).as_deref(),
        summary.as_deref(),
    );

    // Streaming outlet: push model text deltas to the client as soon as they arrive.
    let delta_sink = sink.clone();
    let delta_turn_id = turn_id.to_string();
    let on_text: TextSink = Arc::new(move |delta: &str| {
        delta_sink.notification(ServerNotification::TextDelta(TextDeltaNotification {
            turn_id: delta_turn_id.clone(),
            delta: delta.to_string(),
        }));
    });

    // ---- THE LOOP
    // The tool environment is constructed in exactly one place (see tool_ctx in lib.rs) — the subagent uses
    // the same one (it passes None). The approval channel is built per turn: None in `never` mode, actions needing approval are rejected.
    let ctx = server.tool_ctx(session_id, turn_cwd, server.approval_bridge(turn_id, sink));

    // The tool **start** notification must go out before execution: the client can then draw "calling X".
    // If the graph's node event outlet (inner) is also present, call both — neither blocks the other.
    // Note this is joyczl_loop's Observer (receiving LoopEvent), not the graph's one.
    let tool_started: joyczl_loop::Observer = {
        let sink = sink.clone();
        let turn_id = turn_id.to_string();
        Arc::new(move |event| {
            if let LoopEvent::ToolStart { name, args } = event {
                sink.notification(ServerNotification::ToolStarted(ToolStartedNotification {
                    turn_id: turn_id.clone(),
                    tool: name,
                    args,
                }));
            }
        })
    };
    let observer: Option<joyczl_loop::Observer> = match inner {
        // Running inside the graph: the tool-start notification goes out first, then node events go to the engine to attach node=.
        Some(inner) => {
            let combined: joyczl_loop::Observer = Arc::new(move |event| {
                tool_started(event.clone());
                inner(event);
            });
            Some(combined)
        }
        // No graph: the tool-start notification is the only observer.
        None => Some(tool_started),
    };

    // Run one turn; **only** context overflow is worth a retry after compaction — retrying other errors
    // just yields the same error again (same rule as that comment in graph_route).
    let mut retried = false;
    let result: LoopResult = loop {
        let attempt = joyczl_loop::run(joyczl_loop::Turn {
            client: resolved.client.as_ref(),
            model: &resolved.model,
            system: system.clone(),
            // Fresh start (history + user message) or resume (the checkpointed working memory
            // as-is, `user_message` ignored): both end up as the same `joyczl_loop::Turn`.
            history: history.clone(),
            user_message: prompt_message.clone(),
            user_images: user_images.clone(),
            tools: &server.tools,
            ctx: ctx.clone(),
            max_iterations: settings.max_iterations,
            max_tokens: settings.max_tokens,
            tool_result_budget: crate::tool_result_budget(&settings),
            observer: observer.clone(),
            on_text: Some(on_text.clone()),
            interrupt: interrupt.clone(),
            checkpoint: checkpoint.clone(),
            resume: resume.clone(),
        })
        .await;

        match attempt {
            Ok(result) => break result,
            Err(e) if !retried && is_context_overflow(&e) => {
                eprintln!("(joy) 上下文超了，压缩后重试一次");
                retried = true;
                let (forced_history, forced_summary) = {
                    let _compact_guard = session_write_lock.lock().await;
                    load_history(
                        &server.chat,
                        resolved,
                        session_id,
                        settings.history_turns,
                        0,
                        Some(FORCED_RETRY_TURNS),
                        &server.hooks,
                    )
                    .await
                };
                history = forced_history;
                summary = forced_summary;
                system = build_system(
                    &soul,
                    &resolved.model,
                    &resolved.provider_id,
                    &memory_context,
                    &skills,
                    server.todo.render(session_id).as_deref(),
                    summary.as_deref(),
                );
            }
            Err(e) => {
                return Err(ErrorObject {
                    code: codes::PROVIDER_ERROR,
                    message: format!("模型调用失败：{e}"),
                    data: None,
                })
            }
        }
    };

    Ok(FullTurn {
        result,
        gate: Some(gate_decision),
    })
}

/// The graph's front door.
///
/// Flag off → `Ok(None)`, the caller takes the original path; flag on but the graph yields no answer
/// (engine error, classifier broken, node throws) → also `Ok(None)`, falling back to the ordinary full loop.
/// The cost of a "broken graph" must be latency, never capability.
///
/// The sole exception is the loop itself failing: that error is returned as-is, **no second retry** —
/// a retry would just fail the same way again, wasting double the time.
async fn graph_route(
    server: &Server,
    resolved: &Resolved,
    message: &str,
    frame: TurnFrame<'_>,
) -> Result<Option<Routed>, ErrorObject> {
    let TurnFrame {
        session_id,
        turn_id,
        sink,
        interrupt,
        turn_cwd,
        route_reason,
        // Resumed turns bypass the graph entirely (full_turn directly), so the frame's
        // resume slot never reaches here.
        resume: _,
        checkpoint,
    } = frame;
    if !server.settings().graph_workflows {
        return Ok(None);
    }

    // State only holds JSON, and a loop turn's product (`LoopResult`) is not JSON —
    // so full_agent's **real** result comes back through this side channel; what goes into state is
    // just "I ran".
    let slot: Arc<Mutex<Option<Result<FullTurn, ErrorObject>>>> = Arc::new(Mutex::new(None));

    let classify: ClassifyFn = {
        let client = resolved.client.clone();
        let model = resolved.small_model.clone();
        Arc::new(move |message: String| {
            let (client, model) = (client.clone(), model.clone());
            Box::pin(async move { Ok(classify_message(client.as_ref(), &model, &message).await) })
        })
    };

    let calendar: CalendarFn = {
        let home = server.settings().home.clone();
        Arc::new(move || todays_events(&home))
    };

    let quick: QuickFn = {
        let client = resolved.client.clone();
        let model = resolved.small_model.clone();
        Arc::new(move |state: State| {
            let (client, model) = (client.clone(), model.clone());
            Box::pin(async move {
                let prompt = QUICK_REPLY_PROMPT
                    .replace("{calendar}", state.str("calendar").unwrap_or(""))
                    .replace("{message}", state.str("message").unwrap_or(""));
                let response = client
                    .create(CreateRequest {
                        model,
                        system: None,
                        messages: vec![Message::user_text(prompt)],
                        tools: Vec::new(),
                        max_tokens: 600,
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(response.text())
            })
        })
    };

    let full: FullFn = {
        let server = server.clone();
        let resolved = resolved.clone();
        let sink = sink.clone();
        let (turn_id, session_id) = (turn_id.to_string(), session_id.to_string());
        let turn_cwd = turn_cwd.clone();
        let route_reason = route_reason.clone();
        // The graph's inner loop checkpoints too: its tool rounds are this turn's progress
        // just the same (checkpoint is None only for quick answers, which run no loop).
        let checkpoint = checkpoint.clone();
        let slot = slot.clone();
        Arc::new(move |ctx: NodeCtx| {
            // FullFn is an Fn (the engine may call it multiple times): only clone here; never move the captured values.
            let turn_cwd = turn_cwd.clone();
            let route_reason = route_reason.clone();
            let checkpoint = checkpoint.clone();
            let (server, resolved, sink) = (server.clone(), resolved.clone(), sink.clone());
            let (turn_id, session_id, slot) = (turn_id.clone(), session_id.clone(), slot.clone());
            let interrupt = interrupt.clone();
            Box::pin(async move {
                // Whatever the user said this turn is on the whiteboard — the initial state run_graph receives.
                let message = ctx.state.str("message").unwrap_or_default().to_string();
                // The loop's events go out through the node outlet; the engine attaches node= before forwarding.
                let inner = Some(ctx.inner.clone());
                let outcome = full_turn(
                    &server,
                    &resolved,
                    &message,
                    // Graph-routed turns (triage / gather) are engine-driven continuations:
                    // images already rode on the initial user message.
                    Vec::new(),
                    TurnFrame {
                        session_id: &session_id,
                        turn_id: &turn_id,
                        sink: &sink,
                        interrupt,
                        turn_cwd,
                        route_reason,
                        resume: None,
                        checkpoint,
                    },
                    inner,
                )
                .await;
                // Record first, then decide what to return to the engine: the error only leaves a trace on the
                // engine side; the truth goes out through the side channel (its ErrorObject carries the code).
                let why = outcome.as_ref().err().map(|error| error.message.clone());
                *slot.lock().expect("侧信道锁不该中毒") = Some(outcome);
                match why {
                    Some(why) => Err(why),
                    None => Ok(NodeWrites::new()),
                }
            })
        })
    };

    // The graph is hard-coded next to this file; reaching Err means we wrote it wrong — this turn still
    // takes the ordinary path; don't gamble the user's turn on a graph that cannot be built.
    let graph = match build_triage_graph(classify, calendar, quick, full) {
        Ok(graph) => graph,
        Err(error) => {
            eprintln!("(joy) triage 图没搭起来，这轮走普通路径 —— {error}");
            return Ok(None);
        }
    };

    let observer: Observer = {
        let sink = sink.clone();
        Arc::new(move |event| graph_event(&sink, event))
    };

    let mut state = State::new();
    state.set("message", serde_json::json!(message));

    let report = match run_graph(graph, state, Some(observer), DEFAULT_MAX_STEPS).await {
        // A key collision is a graph bug (parallel nodes wrote the same key); reporting beats silently dropping a write.
        Err(error) => {
            eprintln!("(joy) triage 图跑挂了，这轮走普通路径 —— {error}");
            return Ok(None);
        }
        Ok(report) => report,
    };

    let reason = report
        .state
        .str("triage_reason")
        .unwrap_or_default()
        .to_string();
    let info = |route: GraphRouteKind| GraphInfo {
        workflow: "triage".to_string(),
        route,
        reason: reason.clone(),
        path: report.path.clone(),
    };

    // Whether full_agent ran, check the side channel; if it ran and failed, report that failure honestly.
    // Release the lock before dispatching (this is the function's tail expression; the guard cannot live that long).
    let landed = slot.lock().expect("侧信道锁不该中毒").take();
    match landed {
        Some(Err(error)) => Err(error),
        Some(Ok(turn)) => Ok(Some(Routed {
            info: info(GraphRouteKind::Full),
            turn,
        })),
        // The quick answer wrote its reply directly on the whiteboard.
        None => match report.state.str("reply").filter(|reply| !reply.is_empty()) {
            Some(reply) => Ok(Some(Routed {
                info: info(GraphRouteKind::Quick),
                turn: FullTurn {
                    // One model call, no tools — that is its entirety.
                    result: LoopResult {
                        reply: reply.to_string(),
                        tool_calls: Vec::new(),
                        iterations: 1,
                        usage: Usage::default(),
                        messages: Vec::new(),
                        interrupted: false,
                        guard: joyczl_loop::guard::GuardReport::default(),
                        // The graph finished on its own (loop not used); there is no local estimate to pair with.
                        estimated_input_tokens: 0,
                        observed_input_tokens: 0,
                    },
                    gate: None,
                },
            })),
            // The graph produced nothing — the caller falls back to the ordinary full loop.
            None => Ok(None),
        },
    }
}

/// Translate graph events into protocol notifications.
///
/// Two event kinds are **not** emitted here:
///   - `Inner`: the loop layer already emitted them (text deltas, tool completions). Emitting again,
///     the client would see them twice.
///   - `Route`: which path was taken lands in the turn's meta at the end (`TurnMeta.graph`);
///     a standalone "picked a side" notification carries no information for the user.
fn graph_event(sink: &EventSink, event: GraphEvent) {
    match event {
        GraphEvent::Started { workflow, nodes } => {
            sink.notification(ServerNotification::GraphStarted(GraphStartedNotification {
                workflow,
                nodes,
            }));
        }
        GraphEvent::NodeStarted {
            workflow,
            node,
            visit,
        } => {
            sink.notification(ServerNotification::GraphNodeStarted(
                GraphNodeStartedNotification {
                    workflow,
                    node,
                    visit,
                },
            ));
        }
        GraphEvent::NodeEnded {
            workflow,
            node,
            ms,
            keys,
            error,
        } => {
            sink.notification(ServerNotification::GraphNodeEnded(
                GraphNodeEndedNotification {
                    workflow,
                    node,
                    ms: narrow(ms),
                    keys,
                    error,
                },
            ));
        }
        GraphEvent::Ended {
            workflow,
            ms,
            steps,
            path,
            error,
        } => {
            sink.notification(ServerNotification::GraphEnded(GraphEndedNotification {
                workflow,
                ms: narrow(ms),
                steps,
                path,
                error,
            }));
        }
        GraphEvent::Route { .. } | GraphEvent::Inner { .. } => {}
    }
}

pub(crate) fn build_system(
    soul: &str,
    model: &str,
    provider: &str,
    memory: &str,
    skills: &str,
    todo: Option<&str>,
    summary: Option<&str>,
) -> String {
    let now = Local::now();
    let mut parts = vec![
        soul.to_string(),
        // The agent runs on the user's machine, so it should know the user machine's clock —
        // include the timezone name, or "in 30 minutes" has no answer.
        format!(
            "\nRight now it is {} ({}, UTC{})",
            now.format("%A, %Y-%m-%d %H:%M"),
            now.format("%Z"),
            now.format("%:z")
        ),
        // "What model are you" is every curious user's first question.
        format!(
            "Your model: you are running on '{model}' via the '{provider}' provider, \
             inside Joy, a local-first open-source agent harness."
        ),
    ];
    // The todo list comes before memory and methods: it is **what this run is doing**, which should be seen
    // before background knowledge. And it is injected, not in history — compaction and sliding windows cannot wash it away.
    if let Some(todo) = todo {
        parts.push(format!("\n{todo}"));
    }
    if !memory.is_empty() {
        parts.push(format!("\nRelevant memory:\n{memory}"));
    }
    if !skills.is_empty() {
        parts.push(format!("\nRelevant skill instructions:\n{skills}"));
    }
    if let Some(section) = summary.and_then(joyczl_memory::compaction::summary_section) {
        parts.push(section);
    }
    parts.join("\n")
}

/// The transcript for the judge: the session's recent turns, with this turn's reply appended.
///
/// The judge **only reads the conversation** (no tools, no retrieval): what it must answer is "does this
/// conversation show the goal is met"; looking at anything else only makes it start doing the model's work.
async fn judge_transcript(server: &Server, session_id: &str, reply: &str) -> String {
    let history = server
        .chat
        .session_history(session_id)
        .await
        .unwrap_or_default();
    if history.is_empty() {
        return format!("assistant: {reply}");
    }
    let mut out = String::new();
    let recent = history.len().saturating_sub(8);
    for (role, content) in &history[recent..] {
        out.push_str(&format!("{role}: {content}\n"));
    }
    out.push_str(&format!("assistant: {reply}"));
    out
}

/// Emit one goal-loop notification (`GoalRound` is the only answer to "why another round ran").
fn notify_goal(
    sink: &crate::EventSink,
    turn_id: &str,
    round: i32,
    max_rounds: i32,
    status: &str,
    reason: &str,
) {
    sink.notification(ServerNotification::GoalRound(GoalRoundNotification {
        turn_id: turn_id.to_string(),
        round,
        max_rounds,
        status: status.to_string(),
        reason: reason.to_string(),
    }));
}

/// How many times a single turn may continue because of the Stop hook. The generic mechanism is the goal loop (`goal/set`).
const MAX_STOP_CONTINUATIONS: usize = 1;

/// Take only the most recent N turns, plus a rolling summary.
///
/// Without a sliding window, a long session stuffs the entire history into the prompt every turn until
/// the context explodes; with only a sliding window, the squeezed-out part equals amnesia. So: the squeezed-out
/// old turns are handed to `compaction`, folded into one summary (fail-open; worst case a truncated excerpt),
/// stored with the session, and spliced into the system prompt every turn. Returns `(working memory, summary)`.
/// `token_budget` is the approximate token count this turn **may spend on history**; `window_override`
/// is the "force compaction after context overflow" path: it pins how many turns to keep, ignoring the budget.
#[allow(clippy::too_many_arguments)]
async fn load_history(
    chat: &joyczl_state::Chat,
    resolved: &Resolved,
    session_id: &str,
    history_turns: i32,
    token_budget: usize,
    window_override: Option<usize>,
    hooks: &Option<Arc<joyczl_tools::hooks::Hooks>>,
) -> (Vec<Message>, Option<String>) {
    let pairs = chat.session_history(session_id).await.unwrap_or_default();
    let ceiling = history_turns.max(0) as usize;
    // The turn count is the ceiling; tokens are the gate.
    let window = match window_override {
        Some(forced) => forced,
        None => ceiling.min(joyczl_memory::compaction::turns_that_fit(
            &pairs,
            token_budget,
        )),
    };

    // PreCompact must fire **before acting** (a note after the fact is not "Pre"). Whether compaction will
    // actually happen is decided in one place, `compaction::due` — the hook should not ring when nothing was really compacted.
    let will_compact = joyczl_memory::compaction::due(chat, session_id, &pairs, window).await;
    if will_compact {
        let outcome = crate::fire_hook(
            hooks,
            joyczl_tools::hooks::HookEvent::PreCompact,
            serde_json::json!({ "session_id": session_id, "window": window, "turns": pairs.len() }),
        )
        .await;
        // Observe-only event: the hook's words are a side note and must not block compaction (blocking it could overflow this turn).
        if let Some(note) = outcome.note {
            eprintln!("(joy) PreCompact：{note}");
        }
    }

    let summary = joyczl_memory::compaction::refresh(
        chat,
        resolved.client.as_ref(),
        &resolved.small_model,
        session_id,
        &pairs,
        window,
    )
    .await
    .unwrap_or(None);

    if will_compact {
        crate::fire_hook(
            hooks,
            joyczl_tools::hooks::HookEvent::PostCompact,
            serde_json::json!({
                "session_id": session_id,
                "summary_chars": summary.as_ref().map(String::len).unwrap_or(0),
            }),
        )
        .await;
    }

    let messages = pairs
        .iter()
        .rev()
        .take(window)
        .rev()
        .flat_map(|(user, assistant)| {
            vec![
                Message::user_text(user.clone()),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: assistant.clone(),
                    }],
                },
            ]
        })
        .collect();
    (messages, summary)
}

fn fold_tool_activity(reply: &str, calls: &[joyczl_loop::ToolOutcome]) -> String {
    if calls.is_empty() {
        return reply.to_string();
    }
    let summary = calls
        .iter()
        .map(|c| format!("{}({})", c.name, truncate(&c.output, 200)))
        .collect::<Vec<_>>()
        .join("; ");
    format!("{reply}\n[tools used: {summary}]")
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{head}…")
    }
}

fn new_turn_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("t{nanos}")
}
