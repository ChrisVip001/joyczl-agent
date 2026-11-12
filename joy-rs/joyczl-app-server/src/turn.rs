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

#[path = "turn_driver.rs"]
pub(crate) mod driver;

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
use joyczl_loop::{LoopEvent, LoopResult, Steering};
use joyczl_protocol::{
    codes, ConsolidationCompletedNotification, ErrorObject, GateDecidedNotification, GateDecision,
    GateDecisionKind, GoalRoundNotification, GraphEndedNotification, GraphInfo,
    GraphNodeEndedNotification, GraphNodeStartedNotification, GraphRouteKind,
    GraphStartedNotification, JsonRpcMessage, JsonRpcResponse, RequestId, RetryNotification,
    ServerNotification, TextDeltaNotification, TokenUsage, ToolCallRecord,
    ToolCompletedNotification, ToolDeltaNotification, ToolStartedNotification, ToolStatus,
    TurnCheckpointedNotification, TurnCompletedNotification, TurnMeta, TurnStartParams,
    TurnStartResponse, TurnStartedNotification, TurnSuspendedNotification, JSONRPC_VERSION,
};
use joyczl_provider::{ContentBlock, CreateRequest, Message, Resolved, Role, TextSink, Usage};
use joyczl_state::{RunStatus, TaskRow, TaskStatus};

use crate::{narrow, route, trace, EventSink, Server};

const DEFAULT_SOUL: &str = "You are Joy, a local personal assistant that runs on the user's own machine.
You are concise, warm and proactive. What the user tells you, you remember.

Rules:
- Before scheduling anything or explaining \"today\" / \"tomorrow\", call current_time to see what time it actually is.
- Long-lived facts about people, projects and preferences are distilled into the memory library in the background; use save_note only when the user explicitly says \"remember this\".
- Questions about the past or about a person: search_memory. What you have recorded: list_memory.
- When the user explicitly says \"forget …\", use forget_note.
- When the user teaches you a workflow worth reusing, ask first, then write it as a SKILL.md with create_skill.
- \"Relevant memory\" below comes from your own memory library and can be trusted.
- Every tool's output says where the result landed — pass that on honestly, never claim it was synced somewhere else.
- [tools used: ...] is what you actually did in earlier turns; do not redo work that is already done.";

/// The thin soul: for models that already know how to be an assistant.
///
/// Same three commitments as the long one — check the clock, memory is the library's job,
/// tell the truth about where output landed — without the examples and the per-tool
/// recital a frontier model does not need. Every line here is paid for on every turn.
const THIN_SOUL: &str = " You are Joy, a local personal assistant that runs on the user's own machine. Concise, warm, proactive.

- Need the time or a schedule? Call current_time first; do not guess.
- Long-lived facts about people, projects and preferences are distilled into the memory library in the background; use save_note only when the user explicitly says \"remember this\".
- Past events: search_memory. What you have recorded: list_memory. \"Forget it\": forget_note.
- Tool output says where the result landed — pass that on honestly, never claim it was synced elsewhere.
- [tools used: ...] is what you really did in earlier turns; do not repeat it.";

/// Does this model get the thin soul?
///
/// `auto` treats the hosted vendors as strong and the local ones (ollama / LM Studio) as
/// the ones that need scaffolding — the honest ceiling of what we know without asking the
/// catalog for a parameter count it does not carry. `thin` / `full` override it outright,
/// and `Settings::validate` rejects anything else at startup.
pub(crate) fn thin_soul_for(mode: &str, provider: &str) -> bool {
    match mode {
        "thin" => true,
        "full" => false,
        _ => !matches!(provider, "ollama" | "lmstudio"),
    }
}

/// SOUL.md is the user-editable persona file, created on first run.
///
/// Editing it changes who Joy is — the plainest form of procedural memory. So the file
/// wins: a soul the user wrote is returned as-is, whatever the mode says. The mode only
/// decides which built-in text seeds the file and what an **untouched** default becomes —
/// detecting the default by content, because "you never edited this" is exactly the case
/// where a shorter prompt is a pure win, and overwriting an edited file would be theft.
pub(crate) fn load_soul(home: &std::path::Path, thin: bool) -> String {
    let path = home.join("SOUL.md");
    let seed = if thin { THIN_SOUL } else { DEFAULT_SOUL };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => {
            let _ = std::fs::write(&path, seed);
            return seed.to_string();
        }
    };
    // Untouched default: swap in the seed for this mode. Anything else is the user talking.
    if text.trim() == DEFAULT_SOUL.trim() || text.trim() == THIN_SOUL.trim() {
        if text.trim() != seed.trim() {
            let _ = std::fs::write(&path, seed);
        }
        return seed.to_string();
    }
    text
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
"The model on this configuration ({provider_id}) cannot take images. Declare vision for it: /model add <name> {provider_id} <model> --vision (the same as adding \"vision\": true to that entry in <home>/models.json); to make it true of the whole provider, add \"vision\": true to its row in <home>/models-catalog.json and restart."
        )));
    }
    let home = server.settings().home;
    let home_real = home
        .canonicalize()
        .map_err(|e| bad(format!("The home directory is unavailable: {e}")))?;
    let mut images = Vec::new();
    for attachment in attachments {
        let raw = home.join(&attachment.path);
        // Symlinks resolve first: the real path must stay inside home, no exceptions.
        let path = raw.canonicalize().map_err(|_| {
            bad(format!(
                "Attachment is missing or unreadable: {}",
                attachment.path
            ))
        })?;
        if !path.starts_with(&home_real) {
            return Err(bad(format!(
                "Attachment {} is outside home — an attachment may only reference a file under the upload directory (<home>/uploads/…).",
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
"Attachment {} is not a recognised image type (png / jpg / jpeg / gif / webp); for any other file, put the path in the message and the model will open it with read_file.",
                    attachment.path
                ))
            })?;
        let bytes =
            std::fs::read(&path).map_err(|e| bad(format!("Could not read the attachment: {e}")))?;
        if bytes.len() > IMAGE_MAX_BYTES {
            return Err(bad(format!(
                "Attachment {} is {} bytes, over the per-image limit of {IMAGE_MAX_BYTES}.",
                attachment.path,
                bytes.len()
            )));
        }
        // 像素尺寸也得量：长截图（1120×11100 这种）字节数只有 2 MB 出头，却会被远端
        // 一句 "unsupported image" 挡回来 —— 它既不说原因也不说上限，所以本地先量。
        let (width, height) = crate::image_meta::size(&bytes).ok_or_else(|| {
            bad(format!(
"Attachment {} has no readable dimensions — its header is not a complete png / jpg / gif / webp, so it is probably corrupt, or only had its extension changed.",
                attachment.path
            ))
        })?;
        if width > crate::image_meta::MAX_EDGE || height > crate::image_meta::MAX_EDGE {
            return Err(bad(format!(
"Attachment {} is {width}×{height}, one side over the local conservative limit of {}: a remote vision model would refuse it outright (a bare \"unsupported image\"), so it is caught here. Send a long screenshot in pieces — a few images no taller than {} keep the text legible; squeezing it into one only blurs it.",
                attachment.path,
                crate::image_meta::MAX_EDGE,
                crate::image_meta::MAX_EDGE
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

    let session_id = params
        .session_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "default".to_string());

    // ---- Memory distilled in the background since the last turn is reported here.
    //
    // The worker does its work off the clock, so this is the moment the client hears about
    // it: the same `consolidationCompleted` notification, one turn later than before, at
    // the point where the user can actually act on it ("记住了 3 条新事实").
    if let Some(worker) = server.memory_worker.as_ref() {
        let pending = worker.take_pending();
        if pending > 0 {
            sink.notification(ServerNotification::ConsolidationCompleted(
                ConsolidationCompletedNotification { new_facts: pending },
            ));
        }
    }

    // ---- Long task: if the session has an active one, this turn carries it. The heartbeat
    // (`current_turn_id`) is what `task/suspend`, `task/resume` and the boot reconcile hang on.
    // Read before model resolution: a task may **declare its tier**, and that pins this turn's model.
    let task = server.tasks.active(&session_id).await.ok().flatten();

    // The turn's tier: an explicit `tier` (task API / webhook) wins over the task's declaration;
    // a typo is refused here, before any side effect (a silently-guessed tier would bill the wrong model).
    let tier = route::clean_tier(params.tier.as_deref())?
        .or_else(|| task.as_ref().and_then(|task| task.tier.clone()));

    // Model not configured: spell out "what to do" instead of just dropping a "no key".
    let resolved: Resolved = server.resolved().ok_or_else(|| ErrorObject {
        code: codes::PROVIDER_ERROR,
message: "The model is not configured yet: no API key. One command is enough — /model add <name> <provider> --env VAR (the key goes to the system keychain)."
            .to_string(),
        data: None,
    })?;

    // Pick the model (route.rs): a declared tier always wins; otherwise auto mode decides per
    // message; fixed mode, or "the picked one is already active", return as-is with zero overhead.
    // The fallback and selection reasons go into route_reason, unhidden.
    let (resolved, turn_route) = server
        .resolved_for_turn(resolved, &params.message, tier.as_deref())
        .await;

    // ---- Attachments: staged images, read + base64'd here. Gates on the provider's vision
    // capability and the file's existence **before any side effect** — a bad attachment must
    // not leave a half-turn behind.
    let user_images = resolve_attachments(server, &params, &resolved.provider_id, resolved.vision)?;

    let turn_id = new_turn_id();

    // ---- One driver per session (0.16), claimed **before any side effect**: a turn refused
    // here must not flip a task to `running` or leave a checkpoint behind. The holder's id goes
    // into the error because it is the only thing the caller can act on.
    let (interrupt, steering) =
        server
            .begin_turn(&turn_id, &session_id)
            .map_err(|holder| ErrorObject {
                code: codes::SESSION_BUSY,
                message: format!(
"A turn is already running in this session ({holder}) — wait for it to finish, or add a line with turn/steer; it is picked up on the turn's next round."
                ),
                data: None,
            })?;
    let _guard = TurnGuard {
        server,
        turn_id: turn_id.clone(),
        session_id: session_id.clone(),
    };

    if let Some(task) = &task {
        match server.tasks.set_running(&task.id, &turn_id).await {
            Ok(()) => {
                if let Ok(Some(row)) = server.tasks.get(&task.id).await {
                    crate::task::notify(sink, &row, None);
                }
            }
            Err(e) => eprintln!("(joy) task heartbeat not recorded (the task card keeps showing the old state): {e}"),
        }
    }
    let task_id = task.as_ref().map(|task| task.id.clone());

    // The task's objective half: what must be measurably true, and where its paths live.
    // Parsed once per turn — verification runs after every round, and re-parsing the same JSON
    // each time would cost more than the `stat`s it guards.
    let acceptance = task
        .as_ref()
        .map(|row| joyczl_agent::verify::parse(&row.acceptance_json))
        .unwrap_or_default();
    let task_root = task
        .as_ref()
        .and_then(|row| row.cwd.as_deref())
        .map(std::path::PathBuf::from);

    // ---- P1: an execution plan — opt-in (JOY_PLAN), and only when there is a task to plan for.
    //
    // One model call, before the first round, whose only product is text: the plan is written to
    // the step ledger (`kind = "plan"`) and appended to this round's message. A failure here is
    // logged and ignored — a task must not fail because its plan could not be drawn.
    let mut plan = None;
    if let Some(row) = &task {
        match joyczl_agent::plan::restore(&server.steps, &row.id).await {
            Ok(saved) => {
                plan = saved;
                if plan.is_none() && server.settings().plan_enabled {
                    let labels: Vec<String> = acceptance.iter().map(|item| item.label()).collect();
                    match joyczl_agent::plan::make(
                        &resolved,
                        &row.title,
                        row.goal_condition.as_deref(),
                        &labels,
                        None,
                        1,
                    )
                    .await
                    {
                        Ok(made) => {
                            match joyczl_agent::plan::record(&server.steps, &row.id, &made).await {
                                Ok(()) => plan = Some(made),
                                Err(why) => eprintln!(
                                    "(joy) plan not committed; continuing without it: {why}"
                                ),
                            }
                        }
                        Err(why) => {
                            eprintln!("(joy) no plan this round (the turn runs anyway): {why}")
                        }
                    }
                }
            }
            Err(why) => eprintln!("(joy) saved plan unavailable; no replacement generated: {why}"),
        }
    }

    // ---- The run: one execution of the task, which **is** this turn (a run and the turn that
    // carries it share an id, so a checkpoint inside it always knows which attempt it belongs
    // to). No task, no run — an ordinary chat turn has nothing to account for.
    let run_id = open_run(server, task.as_ref(), &turn_id, &session_id).await;
    // Only meaningful while this turn is the one driving the task: the card's "现在在跑第几轮".
    let round_no = 1i32;
    let max_rounds = task
        .as_ref()
        .map(|row| row.max_rounds.max(1))
        .unwrap_or_else(|| {
            server
                .settings
                .read()
                .expect("the settings lock cannot be poisoned")
                .goal_max_rounds
                .max(1)
        });
    // ---- The checkpoint outlet (shared with the resumed turn — see checkpoint_outlet).
    let checkpoint = checkpoint_outlet(
        &server.checkpoints,
        sink,
        &turn_id,
        &session_id,
        &params.message,
        run_id.as_deref(),
    );

    // Working directory for this turn: validate when provided (fail-closed). Reject before the
    // turn starts, leaving no half-turn behind.
    //
    // Precedence: what **this turn** names → the task's own folder → what the **session**
    // remembers. A task's directory wins over the session's, because "this task owns this
    // folder" would be a lie if the tools writing inside it were bounded by something else.
    let mut turn_cwd = resolve_turn_cwd(&params.cwd)?.or_else(|| task_root.clone());
    if turn_cwd.is_none() {
        turn_cwd = session_cwd(server, &session_id).await;
    }
    // The boundary acceptance checks resolve paths against — same rule, one directory.

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
                message: format!("The SessionStart hook refused this session: {reason}"),
                data: None,
            });
        }
    }
    // The plan travels with the message: context for this round, not a protocol object. Appended
    // last so it sits closest to the work it describes.
    if let Some(current) = &plan {
        message = format!("{message}\n\n{}", current.brief());
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
                    steering: Some(steering.clone()),
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
                            steering: Some(steering.clone()),
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
            // This turn **did not finish** (model died, graph blew up...): close every ledger
            // honestly before leaving — an open checkpoint would be resumed on next boot as if
            // the failure never happened, and the task card would keep saying "running".
            fail_turn(
                server,
                sink,
                &turn_id,
                run_id.as_deref(),
                task_id.as_deref(),
                &e.message,
            )
            .await;
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

    // ---- Continuation: a turn may keep itself alive, a bounded number of times.
    //
    // Two sources, one budget (`JOY_AUTO_CONTINUE_MAX`):
    //   * the human's Stop hook saying "not yet" (exit 2 / decision: block);
    //   * the **built-in gate**: the step cap cut this turn off **and** the model's own todo list
    //     still has `pending` / `in_progress` items. That list is the model's written statement of
    //     what is left, so "your move" is a poor answer when the model itself wrote down the rest.
    //
    // The gate opens only at the cut-off. A turn that ends of its own accord with items left is a
    // turn the model considers finished (a plan it just wrote, say) — driving it on would execute
    // work nobody asked for.
    //
    // This is not a second loop: the goal loop (`/goal`) stays the only thing that auto-continues
    // across rounds on a human-set condition. Both sources here mean "hold off the end".
    //
    // A continuation **carries the working memory** (`Resume`) and a **fresh step slice**. Before
    // this, it started a brand-new loop: history was re-read from `chat_log`, which at that instant
    // did not yet contain this turn's tool results — so the continuation began as if the last steps
    // had never happened, and re-did them. That is the failure Codex's own loop write-up warns
    // about ("continue without the steps and the agent restarts from step 1").
    let settings = server.settings();
    let goal_condition = task.as_ref().and_then(|row| row.goal_condition.clone());
    let auto_continue_max = settings.auto_continue_max.max(0) as usize;

    let mut continues = 0usize;
    // Steps from earlier passes. `full` is **replaced** by the continuation, and the turn's tool
    // results (and their `toolCompleted` notifications, emitted at finalize from `result`) would
    // otherwise cover only the last pass — the client would show a second pass of calls whose
    // earlier results never arrived.
    let mut earlier_calls: Vec<joyczl_loop::ToolOutcome> = Vec::new();
    // The ceiling this pass ran under. A continuation raises it by one slice, and "were we cut
    // off?" is `iterations >= cap` — not a guess about how the model felt about stopping.
    let mut cap = settings.max_iterations.max(1);
    // Set when the turn stops with work still open: either the continuation budget is gone, or the
    // model chose to stop. Both belong in the trace — "stopped short" should be a fact, not a mood.
    let unfinished: Option<String> = loop {
        let payload = serde_json::json!({
            "session_id": session_id,
            "turn_id": turn_id,
            "reply": full.result.reply,
            "iterations": full.result.iterations,
        });
        let outcome =
            crate::fire_hook(&server.hooks, joyczl_tools::hooks::HookEvent::Stop, payload).await;
        let blocked = outcome.blocked;
        // The hook is the human speaking, and it can always demand one more pass. Only when it says
        // nothing does the built-in gate look at facts — and it opens **only when the turn was cut
        // off** (`iterations >= cap`), because that is what "ran out of steps with work left" means.
        //
        // Why not on a natural stop too: a turn that writes a plan and answers "计划如下" is
        // finished work, not unfinished work. Driving that one to keep going would execute a plan
        // nobody asked to execute. A model that stops early with an open todo list is visible in
        // the trace as `unfinished`, and one word from the human continues it.
        //
        // A goal-carrying turn is driven by the goal loop below — one driver per turn.
        let open_todos = if goal_condition.is_none() {
            unfinished_work(server, &session_id, &full.result)
        } else {
            None
        };
        let cut_off = full.result.iterations >= cap;
        let reason = blocked.or_else(|| open_todos.clone().filter(|_| cut_off));
        let Some(reason) = reason else {
            // Stopped of its own accord; if work is still open, say so in the trace rather than
            // hijack the ending.
            break open_todos;
        };
        if continues >= auto_continue_max {
            // Asked for more after we already continued: say so honestly rather than pretend the
            // work is done.
            full.result.reply.push_str(&format!(
                "\n\n(One step short: {reason}. The auto-continue budget is spent — say \"carry on\" and I resume where it stopped.)"
            ));
            break Some(reason);
        }
        continues += 1;
        eprintln!("(joy) auto-continue {continues} of {auto_continue_max}: {reason}");
        let nudge = format!(
            "{reason}\n\nCarry on: continue from where it stopped, do not restate what is done, do not re-plan."
        );
        // Slice, don't restart: one continuation buys `JOY_TURN_STEP_SLICE` more steps, counted
        // on top of the ones already spent.
        cap = full.result.iterations + settings.turn_step_slice.max(1);
        let resume = joyczl_loop::Resume {
            round_id: full.result.round_id.clone(),
            messages: full.result.messages.clone(),
            completed_iterations: full.result.iterations,
            usage: full.result.usage,
            max_iterations: Some(cap),
        };
        let continued = full_turn(
            server,
            &resolved,
            &nudge,
            Vec::new(),
            TurnFrame {
                session_id: &session_id,
                turn_id: &turn_id,
                sink,
                interrupt: Some(interrupt.clone()),
                turn_cwd: turn_cwd.clone(),
                route_reason: turn_route.clone(),
                steering: Some(steering.clone()),
                resume: Some(resume),
                checkpoint: Some(checkpoint.clone()),
            },
            None,
        )
        .await;
        match continued {
            Ok(next) => {
                earlier_calls.append(&mut full.result.tool_calls);
                full = next;
            }
            Err(e) => {
                fail_turn(
                    server,
                    sink,
                    &turn_id,
                    run_id.as_deref(),
                    task_id.as_deref(),
                    &e.message,
                )
                .await;
                return Err(e);
            }
        }
    };

    // Re-attach the earlier passes' calls **before** the last pass's, so the turn reports one
    // request order across the whole turn (and every `toolCompleted` finally has its row).
    if !earlier_calls.is_empty() {
        earlier_calls.append(&mut full.result.tool_calls);
        full.result.tool_calls = earlier_calls;
    }

    let driver::DrivenTask {
        full,
        decision,
        goal_status,
        round_no,
    } = driver::drive_task(
        driver::DriveTask {
            server,
            resolved: &resolved,
            task: task.clone(),
            run_id: run_id.clone(),
            frame: TurnFrame {
                session_id: &session_id,
                turn_id: &turn_id,
                sink,
                interrupt: Some(interrupt.clone()),
                turn_cwd: turn_cwd.clone(),
                route_reason: turn_route.clone(),
                resume: None,
                checkpoint: Some(checkpoint.clone()),
                steering: Some(steering.clone()),
            },
            plan,
            round_no,
            max_rounds,
            started,
            wall_offset_ms: 0,
        },
        full,
    )
    .await?;

    let FullTurn {
        result,
        gate,
        history,
    } = full;

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
            goal_rounds: round_no,
            history,
            started,
            run_id,
            decision: Some(decision),
            continues,
            unfinished,
        },
    )
    .await?;

    // Only after every notification is out does the response get its turn.
    sink.response(JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id,
        result: serde_json::to_value(TurnStartResponse { turn_id }).expect("serialises"),
    }));
    Ok(())
}

/// What this turn decided about the task it was carrying. The *facts* decided it (acceptance,
/// budget, contradictions); this enum is just the verdict travelling to the one place that
/// writes it down.
#[derive(Debug, Clone)]
pub(crate) enum TaskDecision {
    /// Acceptance passed (or there was nothing objective to check) and nothing objected.
    Done,
    /// A machine may not settle this: verification failed, budget spent, side effect unlanded.
    /// The task stays **open** — a human can continue or cancel it.
    NeedsAttention(String),
    WaitingForJobs(Vec<String>),
    /// The loop's own verdict: not achievable, or the round cap was reached.
    Failed(String),
}

/// Everything one finished turn leaves behind — shared by the fresh path and the resumed one,
/// so "a resumed turn gets exactly the same accounting" holds by construction, not by copy-paste.
pub(crate) struct FinishedTurn<'a> {
    /// What history/compaction looked like — the trace writes the before/after in turns.
    pub(crate) history: HistoryStats,
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
    /// The run (task attempt) this turn is; `None` for a task-less turn.
    pub(crate) run_id: Option<String>,
    /// The verdict of this turn's rounds; `None` = nobody decided (a resumed turn).
    pub(crate) decision: Option<TaskDecision>,
    /// How many times this turn continued itself (Stop hook or unfinished work).
    pub(crate) continues: usize,
    /// Why it could not continue further, when it stopped with work still open. `None` = it
    /// stopped because it was done (or was not allowed to try).
    pub(crate) unfinished: Option<String>,
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
        history,
        started,
        run_id,
        decision,
        continues,
        unfinished,
    } = t;
    let settings = server.settings();

    // ---- Checkpoint bookkeeping: the turn reached an end **in this process**. From here on,
    // `open` can only mean "the process died mid-turn" — the invariant the startup reconcile
    // relies on. Two kinds of interruption must not be conflated:
    //   * a stop (turn/interrupt): `abandoned`, never auto-resumed;
    //   * a pause (task/suspend): `suspended`, kept resumable — `task/resume` continues it.
    let paused = server.take_suspending(&turn_id);
    let waiting_for_jobs =
        !paused && !result.interrupted && matches!(decision, Some(TaskDecision::WaitingForJobs(_)));
    let (final_status, suspended_reason) = if paused {
        (
            "suspended",
            Some("Paused; it can resume from the checkpoint"),
        )
    } else if result.interrupted {
        ("abandoned", Some("interrupted by user"))
    } else if result.stop_reason == joyczl_provider::StopReason::UncertainEffect {
        (
            "suspended",
            Some("Tool outcome unknown; reconcile before continuing"),
        )
    } else if waiting_for_jobs {
        ("waiting_jobs", Some(joyczl_state::waits::JOB_WAIT_REASON))
    } else {
        ("done", None)
    };
    if let Err(e) = server.checkpoints.finish(&turn_id, final_status).await {
        eprintln!("(joy) checkpoint close-out not recorded (the turn itself is unaffected): {e}");
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
    //
    // Precedence is deliberate: a human's pause or stop wins over anything a machine concluded.
    // Then the turn's own decision — which came from measurements, never from the reply text.
    let carried_task = server.tasks.by_turn(&turn_id).await.ok().flatten();
    if let Some(task) = &carried_task {
        let control = server.session_write_lock(&format!("task-control:{}", task.id));
        let _control = control.lock().await;
        let latest = server.tasks.get(&task.id).await.ok().flatten();
        let task = latest.as_ref().unwrap_or(task);
        let paused = paused
            || server.take_suspending(&turn_id)
            || (task.status == TaskStatus::Suspended
                && task.suspend_reason.as_deref() != Some(joyczl_state::waits::JOB_WAIT_REASON));
        let (verdict_reason, run_status) = if task.status == TaskStatus::Cancelled {
            let _ = server.checkpoints.finish(&turn_id, "abandoned").await;
            (
                Some("Cancelled by the user".to_string()),
                RunStatus::Cancelled,
            )
        } else if paused {
            let _ = server.checkpoints.finish(&turn_id, "suspended").await;
            let reason = "Paused; it can resume from the checkpoint";
            let _ = server.tasks.suspend(&task.id, Some(reason)).await;
            (Some(reason.to_string()), RunStatus::Suspended)
        } else if result.interrupted {
            let reason = "Interrupted by the user; the task stays where it is";
            let _ = server.tasks.suspend(&task.id, Some(reason)).await;
            (Some(reason.to_string()), RunStatus::Suspended)
        } else {
            match decision {
                Some(TaskDecision::Done) => {
                    let _ = server.tasks.finish(&task.id, TaskStatus::Done).await;
                    (None, RunStatus::Completed)
                }
                Some(TaskDecision::WaitingForJobs(job_ids)) => {
                    let waits = joyczl_state::waits::JobWaits::new(server.pool.clone());
                    let registered = match run_id.as_deref() {
                        Some(id) => {
                            waits
                                .register(&task.id, id, &turn_id, &session_id, &job_ids)
                                .await
                        }
                        None => Err(anyhow::anyhow!("task has no execution identity")),
                    };
                    match registered {
                        Ok(()) => (
                            Some(joyczl_state::waits::JOB_WAIT_REASON.to_string()),
                            RunStatus::Waiting,
                        ),
                        Err(e) => {
                            let reason = format!("Cannot persist background-job wait: {e}");
                            eprintln!("(joy) {reason}");
                            let _ = server.tasks.needs_attention(&task.id, &reason).await;
                            let _ = server.checkpoints.finish(&turn_id, "suspended").await;
                            (Some(reason), RunStatus::Waiting)
                        }
                    }
                }
                Some(TaskDecision::NeedsAttention(reason)) => {
                    eprintln!("(joy) task {} needs your call: {reason}", task.id);
                    let _ = server.tasks.needs_attention(&task.id, &reason).await;
                    (Some(reason), RunStatus::Waiting)
                }
                Some(TaskDecision::Failed(reason)) => {
                    let _ = server.tasks.finish(&task.id, TaskStatus::Failed).await;
                    let _ = server.tasks.set_last_stop(&task.id, &reason).await;
                    (Some(reason), RunStatus::Failed)
                }
                // Nobody decided (a resumed turn): fall back to what the goal loop said, and to
                // the pre-0.14 rule that a task without a condition is done when its turn is.
                None => match goal_status.as_deref() {
                    Some("satisfied") => {
                        let _ = server.tasks.finish(&task.id, TaskStatus::Done).await;
                        (None, RunStatus::Completed)
                    }
                    Some(status @ ("impossible" | "round-limit")) => {
                        let _ = server.tasks.finish(&task.id, TaskStatus::Failed).await;
                        let _ = server.tasks.set_last_stop(&task.id, status).await;
                        (Some(status.to_string()), RunStatus::Failed)
                    }
                    Some("blocked") => {
                        let reason = "Judge failure; stopped here";
                        let _ = server.tasks.needs_attention(&task.id, reason).await;
                        (Some(reason.to_string()), RunStatus::Waiting)
                    }
                    Some(_) => {
                        let _ = server.tasks.clear_turn(&task.id).await;
                        (None, RunStatus::Running)
                    }
                    None => {
                        if task.goal_condition.is_some() {
                            let reason =
                                "The goal did not advance this round; waiting for the next message";
                            let _ = server.tasks.suspend(&task.id, Some(reason)).await;
                            (Some(reason.to_string()), RunStatus::Suspended)
                        } else {
                            let _ = server.tasks.finish(&task.id, TaskStatus::Done).await;
                            (None, RunStatus::Completed)
                        }
                    }
                },
            }
        };
        // The run's account closes with the turn — one execution, one row.
        if let Some(run_id) = &run_id {
            if let Err(e) = server
                .runs
                .close(run_id, run_status, verdict_reason.as_deref())
                .await
            {
                eprintln!("(joy) run record not closed out: {e}");
            }
        }
        if let Ok(Some(row)) = server.tasks.get(&task.id).await {
            crate::task::notify(sink, &row, verdict_reason.as_deref());
        }
    } else if let Some(run_id) = &run_id {
        // A run with no task row behind it (deleted mid-turn): close it rather than leave it open.
        let _ = server
            .runs
            .close(run_id, RunStatus::Failed, Some("the task row is gone"))
            .await;
    }

    for call in &result.tool_calls {
        sink.notification(ServerNotification::ToolCompleted(
            ToolCompletedNotification {
                turn_id: turn_id.clone(),
                tool_use_id: call.tool_use_id.clone(),
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
    let mut reply = fold_tool_activity(&result.reply, &result.tool_calls);
    if result.stop_reason == joyczl_provider::StopReason::MaxTokens && !result.interrupted {
        reply.push_str("\n\n(The output limit was reached; this response is incomplete.)");
    }
    if waiting_for_jobs {
        reply.push_str("\n\n(Background jobs are still running; task completion is pending.)");
    }

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
            // ---- Flattened, so `jq` on a day's trace does not need to remember `meta`'s shape.
            // `callIndex` is the **request** order: with read-only tools running in parallel that
            // is the only order the caller cares about (the wall-clock order is in `durationMs`).
            "tools": result
                .tool_calls
                .iter()
                .enumerate()
                .map(|(index, call)| serde_json::json!({
                    "tool": call.name,
                    "callIndex": index,
                    "durationMs": call.duration_ms,
                    "status": if call.ok() { "ok" } else { "error" },
                }))
                .collect::<Vec<_>>(),
            "retries": retries,
            "tokens": {
                "input": result.usage.input_tokens,
                "output": result.usage.output_tokens,
                // Two numbers for the same request: dividing them is the estimate's error, which
                // is what calibrates the next turn's budget.
                "estimatedInput": result.estimated_input_tokens,
                "observedInput": result.observed_input_tokens,
                // The turn-level cost cap, when one is set (JOY_TURN_TOKEN_BUDGET; 0 = off).
                "turnBudget": settings.turn_token_budget,
            },
            // Compaction is "before/after" in **turns**: how many the session had, how many survived.
            "history": {
                "turnsTotal": history.turns_total,
                "turnsKept": history.turns_kept,
                "compacted": history.compacted,
            },
            // Why it stopped: steps spent, how many times it continued itself, and — when it gave up
            // with work still open — what was left. Without these three, "it stopped short" is an
            // opinion; with them it is a number.
            "steps": result.iterations,
            "continues": continues,
            "unfinished": unfinished,
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
        eprintln!("(joy) usage not recorded in the ledger: {e}");
    }

    // ---- Write section: per-session serialisation. From the history append to the MEMORY.md
    // mirror, this session's writes must not interleave with another concurrent turn's write
    // section — that is the lock's incremental value: the three SQLite guards stop double-write
    // corruption, this stops read/write interleaving. Model calls never hold it.
    let session_write_lock = server.session_write_lock(&session_id);
    let write_guard = session_write_lock.lock().await;

    // Everything steered into this turn, in arrival order — including anything accepted after
    // the loop's last boundary: it is written down even if no model call could see it, so the
    // next turn's history carries it instead of it vanishing.
    let steers = server
        .steering_of(&turn_id)
        .map(|steering| {
            steering
                .lock()
                .expect("the steering lock cannot be poisoned")
                .all()
        })
        .unwrap_or_default();
    server
        .chat
        .append_turn_for(
            user_message,
            &steers,
            &reply,
            &session_id,
            "app-server",
            meta_json.as_deref(),
            carried_task.as_ref().map(|task| task.id.as_str()),
        )
        .await
        .map_err(|e| ErrorObject {
            code: codes::INTERNAL_ERROR,
            message: format!("Could not write the conversation log: {e}"),
            data: None,
        })?;

    // ---- The ledger: the record that `chat_log` above is a *view* of. Same moment, same
    // lock. A failure here is a reporting loss — the reply is already persisted — so it
    // shouts on stderr and the turn carries on, exactly like a tool error.
    let ledger_sink = super::ledger_writer::LedgerSink::new(
        &settings.home,
        settings.ledger_inline_max.max(0) as usize,
    );
    let ledger_entries = super::ledger_writer::entries_for_turn(
        &ledger_sink,
        &session_id,
        &turn_id,
        user_message,
        &result.messages,
        &reply,
    );
    if let Err(e) = server
        .ledger
        .append(&session_id, &turn_id, &ledger_entries, user_message)
        .await
    {
        eprintln!(
            "(joy) ledger write failed (the conversation is stored; this turn is unaffected): {e}"
        );
    }

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
        eprintln!("(joy) observed context size not recorded (this turn is unaffected): {e}");
    }

    if waiting_for_jobs {
        if let Some(task) = &carried_task {
            if let Ok(Some(wait)) = joyczl_state::waits::JobWaits::new(server.pool.clone())
                .get(&task.id)
                .await
            {
                if wait.state == "waiting" {
                    crate::task_waits::arm(server.clone(), sink.clone(), wait);
                }
            }
        }
    }

    // ---- Memory work is the worker's job now: the turn ends when the reply is on disk.
    //
    // The worker decides whether distillation is due (its trigger is unchanged: enough new
    // rows) and mirrors the plaintext library afterwards. Nudging it is all that happens on
    // this clock — no model call, no file rewrite, no held lock. `boot()` without a worker
    // (tests, embedders) keeps the old synchronous path: one function, two callers.
    if let Some(worker) = server.memory_worker.as_ref() {
        worker.wake();
    } else {
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
        if let Err(e) =
            joyczl_memory::export_markdown(&server.facts, &server.episodes, &settings.home).await
        {
            eprintln!("(joy) MEMORY.md mirror failed (memory itself is unaffected): {e}");
        }
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
    /// What history/compaction looked like for this turn — straight into the trace.
    pub(crate) history: HistoryStats,
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
    run_id: Option<&str>,
) -> joyczl_loop::CheckpointSink {
    let (checkpoints, session, user_message) = (
        checkpoints.clone(),
        session_id.to_string(),
        user_message.to_string(),
    );
    let (sink, turn_id) = (sink.clone(), turn_id.to_string());
    // The run is fixed for the whole turn, so the outlet can capture it by value — every
    // boundary of this attempt records the same run id.
    let run = run_id.map(str::to_string);
    Arc::new(move |cp: joyczl_loop::Checkpoint| {
        let (checkpoints, session, user_message) =
            (checkpoints.clone(), session.clone(), user_message.clone());
        let (sink, turn_id) = (sink.clone(), turn_id.clone());
        let run = run.clone();
        Box::pin(async move {
            let messages_json = serde_json::to_string(&cp.messages).unwrap_or_default();
            let usage_json = serde_json::to_string(&cp.usage).unwrap_or_default();
            if let Err(e) = checkpoints
                .save_round(
                    &turn_id,
                    &session,
                    &user_message,
                    &messages_json,
                    cp.iterations,
                    &usage_json,
                    run.as_deref(),
                    Some(&cp.round_id),
                )
                .await
            {
                eprintln!("(joy) checkpoint persistence failed; stopping execution：{e}");
                return Err(format!("checkpoint persistence failed: {e}"));
            }
            sink.notification(ServerNotification::TurnCheckpointed(
                TurnCheckpointedNotification {
                    turn_id,
                    session_id: session,
                    iterations: cp.iterations,
                },
            ));
            Ok(())
        })
    })
}

/// Deregistration guard for a turn: however this turn exits (success, error, interrupted),
/// the cancellation token is removed from the table — a finished turn has nothing left to interrupt.
pub(crate) struct TurnGuard<'a> {
    pub(crate) server: &'a Server,
    pub(crate) turn_id: String,
    pub(crate) session_id: String,
}

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        self.server.finish_turn(&self.turn_id);
        // The session is free again: whatever was queued behind this turn gets a turn of its
        // own now. Spawned because `Drop` cannot await, and safe because `finish_turn` above
        // already released the gate — the drain's turn claims it like any other caller.
        let server = self.server.clone();
        let session_id = self.session_id.clone();
        tokio::spawn(async move { drain_queue(&server, &session_id).await });
    }
}

/// Hand the next queued message to a turn of its own, if there is one.
///
/// Called when a turn ends (`TurnGuard::drop`) and once at boot. One message per call: the turn
/// it starts ends and calls back in, so the queue advances one at a time without any loop here.
pub(crate) async fn drain_queue(server: &Server, session_id: &str) {
    match server.queue.pop(session_id).await {
        Ok(Some(row)) => spawn_queued_turn(server.clone(), row),
        Ok(None) => {}
        Err(e) => eprintln!("(joy) queued instruction could not be claimed: {e}"),
    }
}

/// Run one queued message as its own turn, with **no client attached**.
///
/// `EventSink` already tolerates that ("a send failure has only one possible cause: the writer
/// is gone"), so the notifications simply expire. What must never be silent is a **failure**: a
/// queued turn runs while the human is away, so the error has to land in the transcript.
fn spawn_queued_turn(server: Server, row: joyczl_state::QueuedMessage) {
    tokio::spawn(async move {
        let session_id = row.session_id;
        let message = row.message;
        let (sink, receiver) = EventSink::channel();
        drop(receiver); // nobody is listening; frames expire on send

        let params = TurnStartParams {
            session_id: Some(session_id.clone()),
            message: message.clone(),
            ..TurnStartParams::default()
        };
        let Err(error) = run_turn(&server, params, RequestId::Number(0), &sink).await else {
            if let Err(e) = server.queue.complete(row.id).await {
                eprintln!("(joy) queued instruction completion could not be recorded: {e}");
                return;
            }
            // TurnGuard may have drained before this claim was released.
            drain_queue(&server, &session_id).await;
            return;
        };

        // Losing the session to another client is not a failure of *this message*: put it back in
        // line (it drains when that turn ends) instead of inventing an error the person never
        // caused.
        if error.code == codes::SESSION_BUSY {
            if let Err(e) = server.queue.release(row.id).await {
                eprintln!("(joy) queued message could not be put back in the queue: {e}");
            }
            return;
        }

        // A failure may follow partial execution: never claim that no action ran.
        let note = format!(
            "(this queued turn failed; review prior actions before retrying: {})",
            error.message
        );
        if let Err(e) = server
            .chat
            .append_exchange_for(&message, &note, &session_id, "app-server", None, None)
            .await
        {
            eprintln!("(joy) the queued message's failure could not be recorded either: {e}");
            return;
        }
        if let Err(e) = server.queue.complete(row.id).await {
            eprintln!(
                "(joy) queued instruction failure acknowledgement could not be recorded: {e}"
            );
            return;
        }
        drain_queue(&server, &session_id).await;
    });
}

// ---- Task rounds ---------------------------------------------------------------------------
//
// A round is one pass of THE LOOP. Everything a task needs — the run account, the step ledger,
// the artifacts, the spend and the acceptance verdict — is written at each round boundary, in
// this order: **observe → record → measure → decide**. The judge (a model) only ever speaks
// after the measurements, and never over them.

/// Open this turn's run. The run id **is** the turn id: a run and the turn carrying it are the
/// same attempt, so a checkpoint written inside it always points at the right row. A task-less
/// turn has no run at all — an ordinary chat has nothing to account for.
async fn open_run(
    server: &Server,
    task: Option<&TaskRow>,
    turn_id: &str,
    session_id: &str,
) -> Option<String> {
    let task = task?;
    match server
        .runs
        .start(turn_id, &task.id, Some(session_id), Some(turn_id))
        .await
    {
        Ok(()) => Some(turn_id.to_string()),
        Err(e) => {
            eprintln!("(joy) run record could not be opened (this turn is unaccounted): {e}");
            None
        }
    }
}

/// A run that ended because something broke: close it before the error leaves, otherwise the
/// card keeps saying "running" until the next restart.
async fn abort_run(server: &Server, run_id: Option<&str>, reason: &str) {
    let Some(run_id) = run_id else { return };
    if let Err(e) = server
        .runs
        .close(run_id, RunStatus::Failed, Some(reason))
        .await
    {
        eprintln!("(joy) run record not closed out (the error still reaches you): {e}");
    }
}

/// A turn that failed inside the loop (model died, graph blew up): close the checkpoint, the
/// run and the task honestly, so nothing is left looking "still running".
pub(crate) async fn fail_turn(
    server: &Server,
    sink: &EventSink,
    turn_id: &str,
    run_id: Option<&str>,
    task_id: Option<&str>,
    message: &str,
) {
    if let Err(e) = server.checkpoints.finish(turn_id, "abandoned").await {
        eprintln!("(joy) the failed turn's checkpoint was not marked: {e}");
    }
    abort_run(server, run_id, message).await;
    if let Some(id) = task_id {
        let _ = server.tasks.finish(id, TaskStatus::Failed).await;
        if let Ok(Some(row)) = server.tasks.get(id).await {
            crate::task::notify(sink, &row, Some(message));
        }
    }
}

/// Checkpoint recovery and continuation observe the same round with identical operation keys.
pub(crate) fn observe_round(
    result: &LoopResult,
    turn_id: &str,
    cwd: Option<&std::path::Path>,
) -> joyczl_agent::observe::Observation {
    joyczl_agent::observe::observe(
        &result.messages,
        &format!("{turn_id}:{}", result.round_id),
        1,
        cwd,
    )
}

/// What one round's accounting needs. A struct because the fields would otherwise be seven
/// positional parameters that all mean different things.
pub(crate) struct RoundCtx<'a> {
    pub(crate) task: Option<&'a TaskRow>,
    pub(crate) run_id: Option<&'a str>,
    pub(crate) acceptance: &'a [joyczl_agent::verify::Acceptance],
    /// Where acceptance paths resolve (the task's own directory, else this turn's).
    pub(crate) verify_root: Option<std::path::PathBuf>,
    /// When this round started, for the wall-clock spend.
    pub(crate) started: std::time::Instant,
    pub(crate) wall_offset_ms: i64,
}

/// Record one round and measure it. Returns the verdicts (empty = nothing objective to check).
///
/// Cost: N `stat`s + one INSERT batch, no model call. When a condition fails this saves the
/// judge call the old flow would have spent being talked into "done".
pub(crate) async fn account_round(
    server: &Server,
    ctx: RoundCtx<'_>,
    observation: &joyczl_agent::observe::Observation,
    result: &LoopResult,
) -> Result<Vec<joyczl_agent::verify::CheckResult>, String> {
    let Some(task) = ctx.task else {
        return Ok(Vec::new());
    };

    // 1. What was done: steps (with idempotency keys), files that exist, contradictions.
    joyczl_agent::observe::Recorder {
        steps: &server.steps,
        artifacts: &server.artifacts,
        events: &server.events,
        task_id: &task.id,
        run_id: ctx.run_id,
    }
    .record(observation)
    .await;

    // 2. Record cumulative round watermarks and task/run deltas atomically.
    let wall_ms = ctx
        .wall_offset_ms
        .saturating_add(ctx.started.elapsed().as_millis().min(i64::MAX as u128) as i64);
    if let Some(run_id) = ctx.run_id {
        let ok = observation.steps.iter().filter(|step| step.ok).count() as i64;
        let account = joyczl_state::runs::RoundAccount {
            tokens_in: result.usage.input_tokens,
            tokens_out: result.usage.output_tokens,
            tools_ok: ok,
            tools_failed: observation.steps.len() as i64 - ok,
            wall_ms,
        };
        if let Err(e) = server
            .runs
            .account_round(run_id, &result.round_id, &account)
            .await
        {
            return Err(format!("round accounting failed: {e}"));
        }
    } else {
        return Err(format!(
            "task {} has no run; accounting cannot be committed",
            task.id
        ));
    }

    // 3. The measurement itself.
    let checks = if ctx.acceptance.is_empty() {
        Vec::new()
    } else {
        joyczl_agent::verify::Verifier {
            run_id: ctx.run_id.unwrap_or_default().to_string(),
            root: ctx.verify_root,
            steps: server.steps.clone(),
            task_id: task.id.clone(),
        }
        .run(ctx.acceptance)
        .await
    };

    // 4. The verdict list belongs on the row: the card reads one row, not an event replay.
    if !checks.is_empty() {
        let views: Vec<_> = checks.iter().map(|check| check.view()).collect();
        match serde_json::to_string(&views) {
            Ok(json) => {
                if let Err(e) = server.tasks.set_checks(&task.id, &json).await {
                    eprintln!("(joy) acceptance result not recorded: {e}");
                }
            }
            Err(e) => eprintln!("(joy) acceptance result could not be serialised: {e}"),
        }
        let detail = if joyczl_agent::verify::all_passed(&checks) {
            Some("All acceptance checks passed".to_string())
        } else {
            joyczl_agent::verify::failure_summary(&checks)
        };
        if let Err(e) = server
            .events
            .append(&task.id, ctx.run_id, "verify", detail.as_deref())
            .await
        {
            eprintln!("(joy) acceptance event not recorded: {e}");
        }
    }
    Ok(checks)
}

/// Did this task already spend past one of its caps? One row read, a few comparisons.
///
/// Reaching the cap counts as over it: the next round would spend more, and the whole point of
/// a cap is to stop **before** the money is gone, not after.
pub(crate) fn budget_overrun(task: &TaskRow) -> Option<String> {
    let budget: joyczl_protocol::BudgetView = serde_json::from_str(&task.budget_json).ok()?;
    let mut over = Vec::new();
    if let Some(cap) = budget.wall_ms.filter(|cap| *cap > 0) {
        if task.spent_wall_ms >= cap as i64 {
            over.push(format!(
                "elapsed {:.1}s (cap {:.1}s)",
                task.spent_wall_ms as f64 / 1000.0,
                cap as f64 / 1000.0
            ));
        }
    }
    if let Some(cap) = budget.tokens.filter(|cap| *cap > 0) {
        if task.spent_tokens >= cap as i64 {
            over.push(format!("tokens {} (cap {})", task.spent_tokens, cap));
        }
    }
    if let Some(cap) = budget.tools.filter(|cap| *cap > 0) {
        if task.spent_tools >= cap as i64 {
            over.push(format!("tool calls {} (cap {})", task.spent_tools, cap));
        }
    }
    if over.is_empty() {
        None
    } else {
        Some(format!("Over budget: {}", over.join("; ")))
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
    /// Additions accepted while this turn runs (`turn/steer`). Same channel for every round of
    /// the turn, including stop-hook continuations — a message added during round 2 is still
    /// "about this turn".
    pub(crate) steering: Option<Arc<Mutex<Steering>>>,
}

/// The working directory the session remembers (migration 0018). `None` = it never chose one, or
/// it chose one that has since disappeared — a vanished directory must not silently become "the
/// process cwd": that would turn a missing workspace into writes landing somewhere unexpected.
pub(crate) async fn session_cwd(server: &Server, session_id: &str) -> Option<std::path::PathBuf> {
    let row = server.chat.session(session_id).await.ok().flatten()?;
    let cwd = row.cwd?;
    let path = std::path::PathBuf::from(&cwd);
    if !path.is_dir() {
        eprintln!("(joy) the working directory {cwd} recorded for session {session_id} is gone; this turn has no writable scope");
        return None;
    }
    Some(path)
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
            message: format!("cwd '{text}' must be an absolute path"),
            data: None,
        });
    }
    if !path.is_dir() {
        return Err(ErrorObject {
            code: codes::INVALID_PARAMS,
            message: format!("cwd '{text}' does not exist or is not a directory"),
            data: None,
        });
    }
    Ok(Some(path))
}

/// The longest a pre-loop step (memory gate, retrieval, compaction) may take before Joy
/// proceeds without it. Cancellation is the real protection — this only keeps a hung
/// provider from holding the turn hostage.
const PRELOOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Race a pre-loop step against the user's stop.
///
/// Gate, retrieval and compaction each make a model call *before* the loop starts, and the
/// loop's own `select!` cannot help there: "stop" used to wait behind a summary. `None` means
/// the turn was stopped, and the caller proceeds without that step instead of blocking —
/// the loop sees the same cancellation on its first iteration and answers honestly.
pub(crate) async fn interruptible<T>(
    interrupt: Option<&Arc<joyczl_loop::Interrupt>>,
    what: &str,
    future: impl std::future::Future<Output = T>,
) -> Option<T> {
    match interrupt {
        Some(cancel) => tokio::select! {
            _ = cancel.wait() => {
                eprintln!("(joy) interrupted: no longer waiting for {what}");
                None
            }
            value = future => Some(value),
        },
        None => Some(future.await),
    }
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
        steering,
        ..
    } = frame;
    let settings = server.settings();

    // The vector leg is assembled once per turn and shared: the gate's retrieval and
    // `search_memory` must see the same embedder, or the two disagree about what "similar"
    // means. Assembly failure (no model configured, provider lacks a default endpoint)
    // warns once and both fall back to keywords — one misconfigured knob must not keep the
    // whole conversation from starting.
    let embedder = if settings.embeddings_enabled {
        match joyczl_provider::embed::Embedder::from_settings(&settings) {
            Ok(embedder) => Some(Arc::new(embedder)),
            Err(why) => {
                eprintln!(
                    "(joy) vector search is not configured; this turn uses keywords only: {why}"
                );
                None
            }
        }
    } else {
        None
    };

    // ---- Retrieval gate: does this turn consult memory?
    //
    // A resumed turn skips the gate: the memory context that mattered is already baked into the
    // checkpointed working memory — re-gating would spend a small-model call to re-decide a past.
    let (gate_decision, memory_context) = match &resume {
        Some(_) => (
            GateDecision {
                decision: GateDecisionKind::Skip,
                reason: "resumed from checkpoint — a resume does not pass the gate again"
                    .to_string(),
                query: None,
            },
            String::new(),
        ),
        None => {
            // The gate is a small-model call; a stop must not wait behind it. Stopped before
            // the answer: skip retrieval and let the loop report the interruption.
            let quick = interruptible(
                interrupt.as_ref(),
                "memory gate",
                tokio::time::timeout(
                    PRELOOP_TIMEOUT,
                    joyczl_memory::gate::should_retrieve(
                        resolved.client.as_ref(),
                        &resolved.small_model,
                        message,
                    ),
                ),
            )
            .await;
            match quick {
                Some(quick) => {
                    let decision = match quick {
                        Ok(decision) => decision,
                        Err(_) => joyczl_memory::gate::Decision::fail_open(
                            format!(
                                "the memory gate did not answer within {}s; failing open",
                                PRELOOP_TIMEOUT.as_secs()
                            ),
                            message,
                        ),
                    };
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
                        // Retrieval is the expensive half: stopped mid-way, the turn continues
                        // with no memory and the loop answers "interrupted" on its first round.
                        interruptible(
                            interrupt.as_ref(),
                            "memory retrieval",
                            tokio::time::timeout(
                                PRELOOP_TIMEOUT,
                                joyczl_memory::retrieve_context(
                                    &server.facts,
                                    &server.episodes,
                                    &decision.query,
                                    settings.retrieval_top_k.max(1) as u32,
                                    embedder.as_deref(),
                                ),
                            ),
                        )
                        .await
                        .and_then(|inner| inner.ok())
                        .and_then(|text| text.ok())
                        .unwrap_or_default()
                    } else {
                        String::new()
                    };
                    (gate_decision, memory_context)
                }
                // Stopped before the gate answered: no retrieval, no more model calls. The
                // loop checks the same token on its first iteration and returns `interrupted`.
                None => (
                    GateDecision {
                        decision: GateDecisionKind::Skip,
                        reason: "interrupted: skipped the memory gate and retrieval".to_string(),
                        query: None,
                    },
                    String::new(),
                ),
            }
        }
    };

    // ---- Procedural memory: SKILL.md progressive disclosure. Frontmatter is always scanned (cheap),
    // the body enters the prompt only when the message matches.
    let mut skill_loader = joyczl_memory::skills::SkillLoader::new(
        joyczl_memory::skills::SkillLoader::dirs_for(&settings.home),
    );
    // The body cap is a prompt-shape knob: a 6 KB SKILL.md dropped in whole because the
    // message said "deploy" is the same context problem as an unpruned memory catalog.
    let skill_hits = skill_loader.hits_capped(message, settings.skill_body_tokens.max(1) as usize);
    let skills = skill_hits.section;
    // The catalog is the other half of progressive disclosure: names and one-liners ride
    // along every turn, bodies only when chosen (by keyword here, by `read_skill` when the
    // model decides for itself).
    let skill_index = skill_loader.index_text(settings.skill_index_tokens.max(1) as usize);
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
    let soul = load_soul(
        &settings.home,
        thin_soul_for(&settings.soul_mode, &resolved.provider_id),
    );
    // Per-session tool table: deferred activation is a property of this conversation, and
    // the loop re-reads `schemas()` every round, so an activation lands on the next round.
    // Eager mode (`JOY_TOOL_POLICY=eager`, the default) leaves the table exactly as built.
    let mut turn_tools = server
        .tools
        .session_view_with(server.activation_set(session_id));
    if settings.tool_policy == "deferred" {
        turn_tools.defer_all_but_always();
    }
    let reserve = joyczl_provider::tokens::estimate_text(&soul)
        + joyczl_provider::tokens::estimate_text(&memory_context)
        + joyczl_provider::tokens::estimate_text(&skills)
        + joyczl_provider::tokens::estimate_tools(&turn_tools.schemas())
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
    let Loaded {
        messages: mut history,
        mut summary,
        stats: mut history_stats,
    } = if resume.is_some() {
        Loaded::empty()
    } else {
        {
            let _compact_guard = session_write_lock.lock().await;
            // Compaction calls the model too: it is raced against the stop and bounded, so
            // "stop" never queues behind a summary.
            let loaded = interruptible(
                interrupt.as_ref(),
                "compaction",
                tokio::time::timeout(
                    PRELOOP_TIMEOUT,
                    load_history(
                        &server.chat,
                        resolved,
                        session_id,
                        settings.history_turns,
                        history_budget,
                        None,
                        &server.hooks,
                    ),
                ),
            )
            .await;
            match loaded {
                Some(Ok(loaded)) => loaded,
                Some(Err(_)) => {
                    eprintln!(
                        "(joy) compaction took over {}s; this turn goes without history",
                        PRELOOP_TIMEOUT.as_secs()
                    );
                    Loaded::empty()
                }
                // Stopped while compacting: the loop will not run, and handing it a
                // half-built history would be worse than an empty one.
                None => Loaded::empty(),
            }
        }
    };
    // The catalog a new session carries (index layer). Read once per turn — it is a small
    // regenerated file, and the cost of a stat + read is far below a single prompt token.
    let memory_index = joyczl_memory::bundle::Bundle::new(&settings.home)
        .index_text()
        .unwrap_or_default();

    // The repository's own conventions, read from the folder the human picked. No folder ⇒
    // nothing to read (and no guessing at the process cwd, which is wherever the server
    // happened to start — that is not the human's choice).
    let instructions = turn_cwd.as_deref().and_then(crate::agents_md::discover);
    let mut prompt = build_system(PromptParts {
        soul: &soul,
        model: &resolved.model,
        provider: &resolved.provider_id,
        memory: &memory_context,
        memory_index: Some(memory_index.as_str()),
        skill_index: Some(skill_index.as_str()),
        skills: &skills,
        todo: server.todo.render(session_id).as_deref(),
        summary: summary.as_deref(),
        instructions: instructions.as_deref(),
        capabilities: capability_note(&settings).as_deref(),
    });

    // ---- The meter: what this turn's prompt actually costs, section by section.
    //
    // Measured here because this is the only place all six sections exist at once; read by
    // the memory center. It is an estimate (cl100k) — the same one the budget uses, so the
    // meter and the compaction trigger never disagree about how full the window is.
    let usage = crate::memory_center::usage_of(
        vec![
            (
                "soul",
                joyczl_provider::tokens::estimate_text(&prompt.stable),
            ),
            (
                "memory",
                joyczl_provider::tokens::estimate_text(&memory_context)
                    + joyczl_provider::tokens::estimate_text(&memory_index),
            ),
            (
                "skills",
                joyczl_provider::tokens::estimate_text(&skill_index)
                    + joyczl_provider::tokens::estimate_text(&skills),
            ),
            (
                "tools",
                joyczl_provider::tokens::estimate_tools(&turn_tools.schemas()),
            ),
            (
                "history",
                joyczl_provider::tokens::estimate_messages(&history),
            ),
            (
                "summary",
                summary
                    .as_deref()
                    .map(joyczl_provider::tokens::estimate_text)
                    .unwrap_or(0),
            ),
        ],
        budget_tokens(resolved, &settings, factor),
        settings.context_warn_ratio,
    );
    if let Ok(mut map) = server.context_usage.lock() {
        map.insert(session_id.to_string(), usage);
    }

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
    let mut ctx = server.tool_ctx(session_id, turn_cwd, server.approval_bridge(turn_id, sink));
    ctx.task_run_id = server
        .runs
        .get(turn_id)
        .await
        .map_err(|e| ErrorObject {
            code: codes::INTERNAL_ERROR,
            message: format!("Cannot resolve task execution identity: {e}"),
            data: None,
        })?
        .map(|run| run.id);
    ctx.activate = Some(turn_tools.activator());
    ctx.embedder = embedder;

    // The tool **start** notification must go out before execution: the client can then draw "calling X".
    // If the graph's node event outlet (inner) is also present, call both — neither blocks the other.
    // Note this is joyczl_loop's Observer (receiving LoopEvent), not the graph's one.
    let tool_started: joyczl_loop::Observer = {
        let sink = sink.clone();
        let turn_id = turn_id.to_string();
        Arc::new(move |event| match event {
            LoopEvent::ToolStart {
                tool_use_id,
                name,
                args,
            } => {
                sink.notification(ServerNotification::ToolStarted(ToolStartedNotification {
                    turn_id: turn_id.clone(),
                    tool_use_id,
                    tool: name,
                    args,
                }));
            }
            // Live progress from a running tool: forwarded as-is, never buffered — the
            // `ToolCompleted` that follows is the real result.
            LoopEvent::ToolDelta { tool_use_id, chunk } => {
                sink.notification(ServerNotification::ToolDelta(ToolDeltaNotification {
                    turn_id: turn_id.clone(),
                    tool_use_id,
                    chunk,
                }));
            }
            _ => {}
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
            system_prefix: Some(prompt.stable.clone()),
            client: resolved.client.as_ref(),
            model: &resolved.model,
            system: prompt.tail.clone(),
            // Fresh start (history + user message) or resume (the checkpointed working memory
            // as-is, `user_message` ignored): both end up as the same `joyczl_loop::Turn`.
            history: history.clone(),
            user_message: prompt_message.clone(),
            user_images: user_images.clone(),
            tools: &turn_tools,
            ctx: ctx.clone(),
            // A continuation carries its own ceiling: it resumes with the steps already spent, so
            // the session cap would leave it zero room. `None` = the session setting, as always.
            max_iterations: resume
                .as_ref()
                .and_then(|point| point.max_iterations)
                .unwrap_or(settings.max_iterations),
            max_tokens: settings.max_tokens,
            // 0 = off; the loop treats `None` and 0 the same, but converting here keeps the
            // "off" decision in one place (the config).
            token_budget: Some(settings.turn_token_budget)
                .filter(|budget| *budget > 0)
                .map(|budget| budget as usize),
            tool_result_budget: crate::tool_result_budget(&settings),
            observer: observer.clone(),
            on_text: Some(on_text.clone()),
            interrupt: interrupt.clone(),
            checkpoint: checkpoint.clone(),
            steering: steering.clone(),
            resume: resume.clone(),
        })
        .await;

        match attempt {
            Ok(result) => break result,
            Err(e) if !retried && is_context_overflow(&e) => {
                eprintln!("(joy) context overflowed; compacting and retrying once");
                retried = true;
                let forced = {
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
                history = forced.messages;
                summary = forced.summary;
                history_stats = forced.stats;
                // Only the tail changes after a forced compaction — the stable prefix is the
                // same bytes as before, so the cache still serves it.
                prompt = build_system(PromptParts {
                    soul: &soul,
                    model: &resolved.model,
                    provider: &resolved.provider_id,
                    memory: &memory_context,
                    memory_index: Some(memory_index.as_str()),
                    skill_index: Some(skill_index.as_str()),
                    skills: &skills,
                    todo: server.todo.render(session_id).as_deref(),
                    summary: summary.as_deref(),
                    instructions: instructions.as_deref(),
                    capabilities: capability_note(&settings).as_deref(),
                });
            }
            Err(e) => {
                return Err(ErrorObject {
                    code: codes::PROVIDER_ERROR,
                    message: format!("Model call failed: {e}"),
                    data: None,
                })
            }
        }
    };

    Ok(FullTurn {
        result,
        gate: Some(gate_decision),
        history: history_stats,
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
        steering,
    } = frame;
    if !server.settings().graph_workflows {
        return Ok(None);
    }

    // Task execution always uses the durable full loop; quick chat does not create a
    // working-memory checkpoint or load task context.
    match server.runs.get(turn_id).await {
        Ok(Some(_)) => return Ok(None),
        Ok(None) => {}
        Err(e) => {
            return Err(ErrorObject {
                code: codes::INTERNAL_ERROR,
                message: format!("Cannot resolve task route: {e}"),
                data: None,
            })
        }
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
        let server = server.clone();
        let session_id = session_id.to_string();
        let soul = load_soul(
            &server.settings().home,
            thin_soul_for(&server.settings().soul_mode, &resolved.provider_id),
        );
        Arc::new(move |state: State| {
            let server = server.clone();
            let session_id = session_id.clone();
            let soul = soul.clone();
            let (client, model) = (client.clone(), model.clone());
            Box::pin(async move {
                let prompt = QUICK_REPLY_PROMPT
                    .replace("{calendar}", state.str("calendar").unwrap_or(""))
                    .replace("{message}", state.str("message").unwrap_or(""));
                let mut history = Vec::new();
                match server.chat.messages(&session_id, None, 12).await {
                    Ok(rows) => {
                        let mut remaining = 8_000usize;
                        // Keep newest context within the cap, then restore chronological order.
                        for row in rows {
                            let text: String =
                                row.content.chars().take(remaining.min(2_000)).collect();
                            remaining = remaining.saturating_sub(text.chars().count());
                            if row.role == "user" {
                                history.push(Message::user_text(text));
                            } else if row.role == "assistant" {
                                history.push(Message {
                                    role: joyczl_provider::Role::Assistant,
                                    content: vec![joyczl_provider::ContentBlock::Text { text }],
                                });
                            }
                            if remaining == 0 {
                                break;
                            }
                        }
                        history.reverse();
                    }
                    Err(e) => eprintln!("(joy) quick reply history unavailable: {e}"),
                }
                history.push(Message::user_text(prompt));
                let response = client
                    .create(CreateRequest {
                        cache_prefix: None,
                        model,
                        system: Some(soul),
                        messages: history,
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
            let steering = steering.clone();
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
                        steering,
                    },
                    inner,
                )
                .await;
                // Record first, then decide what to return to the engine: the error only leaves a trace on the
                // engine side; the truth goes out through the side channel (its ErrorObject carries the code).
                let why = outcome.as_ref().err().map(|error| error.message.clone());
                *slot
                    .lock()
                    .expect("the side-channel lock cannot be poisoned") = Some(outcome);
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
            eprintln!("(joy) the triage graph did not assemble; taking the plain path — {error}");
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
            eprintln!("(joy) the triage graph failed; taking the plain path — {error}");
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
    let landed = slot
        .lock()
        .expect("the side-channel lock cannot be poisoned")
        .take();
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
                    // The graph's quick path never loaded history: nothing to report.
                    history: HistoryStats::default(),
                    // One model call, no tools — that is its entirety.
                    result: LoopResult {
                        stop_reason: joyczl_provider::StopReason::EndTurn,
                        round_id: format!("{turn_id}:quick"),
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

/// Every section the system prompt can carry, in one place.
///
/// A struct, not nine positional arguments: the order used to be the only thing keeping
/// "memory" and "memory_index" apart, and the split below turns on knowing exactly which
/// sections are stable — a named field makes that a refactor, not a hunting trip through
/// call sites.
pub(crate) struct PromptParts<'a> {
    pub soul: &'a str,
    pub model: &'a str,
    pub provider: &'a str,
    pub memory: &'a str,
    pub memory_index: Option<&'a str>,
    /// Every skill's name plus its one-line description — small enough to ride along on
    /// every turn, so the model **chooses** a skill instead of hoping a keyword hits.
    pub skill_index: Option<&'a str>,
    /// The bodies of the skills whose trigger fired this turn (`$name` or a keyword match).
    pub skills: &'a str,
    pub todo: Option<&'a str>,
    pub summary: Option<&'a str>,
    /// The repository's own conventions (`AGENTS.md` upwards from the working directory).
    /// Sits in the dynamic half: it changes when the human switches project, not per turn,
    /// but it must not be part of the cached stable prefix either.
    pub instructions: Option<&'a str>,
    /// What this machine **cannot** do right now (no shell, no file writes). Said once per
    /// turn, in the prompt, because the alternative is the model discovering it by trial:
    /// a turn asked to clean up calendar events had no shell and no delete tool, so it spent
    /// eight iterations reading this repository's docs to find out where events live.
    pub capabilities: Option<&'a str>,
}

/// The system prompt cut in two at the cache boundary.
///
/// `stable` is byte-identical for the whole session (soul + model identity); `tail` is
/// everything that moves — the day, the todo list, memory hits, the skill catalog, the
/// rolling summary. Providers that want an explicit cache boundary (Anthropic) mark the
/// end of `stable`; the ones with automatic prefix caching simply need `stable` never to
/// move. Putting the clock in `tail` is the whole reason it is day-granular there: a
/// minute-level timestamp would invalidate the prefix sixty times an hour.
pub(crate) struct SystemPrompt {
    pub stable: String,
    pub tail: String,
}

impl SystemPrompt {
    /// One string, prefix first — for anyone who only wants to assert on the whole text
    /// (`full_turn` and the subagent pass the two halves through separately, so the cache
    /// boundary survives).
    #[cfg(test)]
    pub fn full(&self) -> String {
        if self.tail.is_empty() {
            self.stable.clone()
        } else {
            format!("{}\n{}", self.stable, self.tail)
        }
    }
}

/// What this machine cannot do right now, phrased as an instruction.
///
/// Two switches decide it, and both are off by default: `JOY_EXEC` (shell) and `JOY_WRITE`
/// (files). Without a line like this the model has no way to tell "the tool is missing" from
/// "I have not found the right tool yet" — it reads directories, greps docs and spends the
/// budget. Both Claude Code and Codex put their permission scope in the prompt for the same
/// reason; the difference is that theirs usually says *what is allowed*.
///
/// `None` when nothing is missing: the common case stays free.
pub(crate) fn capability_note(settings: &joyczl_config::Settings) -> Option<String> {
    let mut missing: Vec<&str> = Vec::new();
    if !settings.exec_enabled {
        missing.push("- running commands (shell) — off until the user sets `JOY_EXEC=1`");
    }
    if !settings.write_enabled {
        missing.push("- writing or editing files — off until the user sets `JOY_WRITE=1`");
    }
    if missing.is_empty() {
        return None;
    }
    Some(format!(
"What this machine cannot do right now:\n{}\nIf a request needs one of these, say so in one sentence and stop — do not approximate it with the tools you do have, and do not go reading source code to find a workaround.",
        missing.join("\n")
    ))
}

pub(crate) fn build_system(parts_in: PromptParts<'_>) -> SystemPrompt {
    let PromptParts {
        soul,
        model,
        provider,
        memory,
        memory_index,
        skill_index,
        skills,
        todo,
        summary,
        instructions,
        capabilities,
    } = parts_in;
    let now = Local::now();
    // Stable half: what Joy is and what it runs on. Nothing here depends on the turn.
    let stable = format!(
"{soul}\nYour model: you are running on '{model}' via the '{provider}' provider, inside Joy, a local-first open-source agent harness."
    );

    // Dynamic half, in the order the model should read it: what time it is, what this run
    // is doing, what it remembers, what it could load.
    //
    // The clock is day-granular on purpose. `current_time` answers "what time is it" exactly;
    // a minute-stamped system prompt would break the cache boundary every turn to say
    // something the tool says better.
    let mut parts = vec![
        // The agent runs on the user's machine, so it should know the user machine's clock —
        // include the timezone name, or "in 30 minutes" has no answer.
        format!(
            "\nRight now it is {} ({}, UTC{})",
            now.format("%A, %Y-%m-%d"),
            now.format("%Z"),
            now.format("%:z")
        ),
    ];
    // Capability scope first: what is *missing* shapes every plan that follows. Codex puts its
    // sandbox permissions in the prompt for the same reason — a model that knows "no shell here"
    // says so in one sentence instead of burning a turn finding out.
    if let Some(capabilities) = capabilities {
        parts.push(format!("\n{capabilities}"));
    }
    // The todo list comes before memory and methods: it is **what this run is doing**, which should be seen
    // before background knowledge. And it is injected, not in history — compaction and sliding windows cannot wash it away.
    if let Some(todo) = todo {
        parts.push(format!("\n{todo}"));
    }
    if !memory.is_empty() {
        parts.push(format!("\nRelevant memory:\n{memory}"));
    }
    // The library's catalog: one line per page (title · kind — one-line summary), bodies on
    // demand via `read_memory_page`. This is the index layer: a new session carries the
    // table of contents, not the encyclopedia. Capped, because a catalog nobody prunes
    // becomes the context problem it was meant to solve.
    if let Some(index) = memory_index {
        if !index.trim().is_empty() {
            parts.push(format!(
                "\nMemory library (catalog only — call read_memory_page for a page):\n{index}"
            ));
        }
    }
    // The skill catalog always rides along; the bodies only for the skills this message
    // reached. Same rule as the memory catalog: the table of contents, not the book.
    if let Some(index) = skill_index {
        if !index.trim().is_empty() {
            parts.push(format!(
                "\nSkills (catalog only — call read_skill for a body when one fits):\n{index}"
            ));
        }
    }
    if !skills.is_empty() {
        parts.push(format!("\nRelevant skill instructions:\n{skills}"));
    }
    // Project conventions before memory: "how this repo works" outranks "what happened
    // before", and both lose to the human's message (the section says so out loud).
    if let Some(instructions) = instructions {
        parts.push(instructions.to_string());
    }
    if let Some(section) = summary.and_then(joyczl_memory::compaction::summary_section) {
        parts.push(section);
    }
    SystemPrompt {
        stable,
        tail: parts.join("\n"),
    }
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

/// Is there work this turn itself declared unfinished?
///
/// The todo list is the model's **own** written statement of what is left, so it is the one
/// "are you done?" signal that costs no model call and cannot be talked around. Three rules keep
/// it from becoming a loop driver:
///
/// * `interrupted` means the human said "enough" — that is not unfinished work;
/// * no tool call this turn means a conversation, and a stale board from an earlier turn must not
///   drag a chat turn back to work it never reopened;
/// * a goal-carrying turn is driven by the goal loop instead (checked by the caller).
fn unfinished_work(
    server: &Server,
    session_id: &str,
    result: &joyczl_loop::LoopResult,
) -> Option<String> {
    if result.interrupted || result.tool_calls.is_empty() {
        return None;
    }
    let list = server.todo.read(session_id)?;
    let open: Vec<&joyczl_tools::todo::TodoItem> = list
        .items
        .iter()
        .filter(|item| {
            matches!(
                item.status,
                joyczl_tools::todo::TodoStatus::Pending
                    | joyczl_tools::todo::TodoStatus::InProgress
            )
        })
        .collect();
    if open.is_empty() {
        return None;
    }
    let mut listed: Vec<String> = open
        .iter()
        .take(3)
        .map(|item| format!("{} ({})", item.content, item.status.as_str()))
        .collect();
    if open.len() > listed.len() {
        listed.push(format!("and {} more", open.len() - listed.len()));
    }
    Some(format!(
        "{} to-do items are still open: {}",
        open.len(),
        listed.join("；")
    ))
}

/// Take only the most recent N turns, plus a rolling summary.
///
/// Without a sliding window, a long session stuffs the entire history into the prompt every turn until
/// the context explodes; with only a sliding window, the squeezed-out part equals amnesia. So: the squeezed-out
/// What one turn's history (and its compaction) looked like — the numbers `traces/*.jsonl` wants.
///
/// All of them are "before/after" in **turns**, not tokens: what compaction actually decides is
/// how many turns survive, and the token estimate that drove that decision is already recorded
/// on the result (`estimatedInputTokens` / `observedInputTokens`).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct HistoryStats {
    pub(crate) turns_total: usize,
    pub(crate) turns_kept: usize,
    pub(crate) compacted: bool,
}

/// The result of loading history: what goes into the prompt, the summary, and what it cost.
pub(crate) struct Loaded {
    pub(crate) messages: Vec<Message>,
    pub(crate) summary: Option<String>,
    pub(crate) stats: HistoryStats,
}

impl Loaded {
    /// No history (first turn / resume / compaction failed). Stats are zero, which is honest.
    fn empty() -> Self {
        Self {
            messages: Vec::new(),
            summary: None,
            stats: HistoryStats::default(),
        }
    }
}

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
) -> Loaded {
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
    Loaded {
        messages,
        summary,
        stats: HistoryStats {
            turns_total: pairs.len(),
            turns_kept: window,
            compacted: will_compact,
        },
    }
}

/// Fold what the tools did into the assistant text that gets written to `chat_log`.
///
/// This is the **only** trace of a tool call that survives into the next turn's history (the
/// conversation is text-level; tool_use/tool_result blocks are rebuilt per turn). So the line
/// has to carry three things: what ran, how much came back, and — when the output was spilled
/// to disk — **where to read it again**. Without the address a long task re-reads the same
/// files every turn, because the model can see it learned something but not what.
pub(crate) fn fold_tool_activity(reply: &str, calls: &[joyczl_loop::ToolOutcome]) -> String {
    if calls.is_empty() {
        return reply.to_string();
    }
    let summary = calls.iter().map(fold_one).collect::<Vec<_>>().join("; ");
    format!("{reply}\n[tools used: {summary}]")
}

/// One call, one line: `name(1.2k 字, 全量在 spill/20261112/exec-….txt: 第一行…)`.
fn fold_one(call: &joyczl_loop::ToolOutcome) -> String {
    let chars = call.output.chars().count();
    let first = call.output.lines().next().unwrap_or("").trim();
    let head = truncate(first, 160);
    match spill_address(&call.output) {
        Some(address) => format!(
            "{}({} chars, full output at {address}; first line: {})",
            call.name, chars, head
        ),
        None => format!("{}({} chars: {})", call.name, chars, head),
    }
}

/// The spill address inside a tool result, if it carries one.
///
/// Parsed back out of the text instead of threaded through the loop: the text the model read
/// is exactly the text folded here, so there is one source of truth and no second field to
/// keep in sync (`joyczl_tools::spill` writes "...完整输出在 <path>）").
fn spill_address(output: &str) -> Option<&str> {
    const MARKERS: &[&str] = &["完整输出在 ", "全量输出在 ", "full output at "];
    for marker in MARKERS {
        let Some(start) = output.rfind(marker) else {
            continue;
        };
        let rest = &output[start + marker.len()..];
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '）' || c == ')' || c == '，' || c == '。')
            .unwrap_or(rest.len());
        let address = rest[..end].trim();
        if !address.is_empty() {
            return Some(address);
        }
    }
    None
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
