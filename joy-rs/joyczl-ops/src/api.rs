//! The three dashboard endpoints.
//!
//! Only three, because that is all a dashboard does: **see the picture**, **dig history**,
//! **talk**. Everything is translated from app-server reads and `turn/start`; no business
//! logic is added here — new business belongs on the protocol side.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Extension, Query};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Local;
use futures_util::stream::{self, Stream};
use joyczl_protocol::{
    codes, methods, ApprovalLogResponse, ApprovalRespondParams, ApprovalRespondResponse,
    ConfigReadResponse, DashboardData, ErrorObject, GoalSetParams, GoalSetResponse,
    MemoryListEpisodesResponse, MemoryListResponse, ModelProfilesResponse, ModelRemoveParams,
    ModelRemoveResponse, ModelSaveParams, ModelSaveResponse, ScheduleListResponse,
    ServerNotification, SessionListResponse, SessionMessagesParams, SessionMessagesResponse,
    TaskActionParams, TaskCreateParams, TaskEnvelope, TaskListResponse, TaskViewResponse,
    TodoListView, TurnInterruptParams, TurnInterruptResponse, TurnStartParams, TurnStartResponse,
    UsageSummaryResponse,
};
use serde_json::json;
use tokio::sync::{broadcast, mpsc};

use crate::AppServer;

/// The first screen fetches only this many recent entries: the dashboard must show "the
/// state right now" at a glance; paging is another matter (the protocol cursor is for that).
const SESSION_LIMIT: u32 = 20;
const FACT_LIMIT: u32 = 30;
const EPISODE_LIMIT: u32 = 20;

/// How many not-yet-consumed notifications may pile up. **Unbounded** on purpose: bounded
/// would stall the forwarding task on send, leaving the broadcast channel unconsumed —
/// intermediate `textDelta`s get dropped, possibly along with `turnCompleted`, and that
/// SSE turn would never end. The unbounded cap is one full turn's worth of notifications,
/// and the channel is torn down when the turn ends.
const TURN_BUFFER: usize = 1024;

/// Cross-device sync ping: a notification distilled to "what happened, where". A ping is
/// a knock on the door, **not the mail** — the receiver pulls `/api/data` for details.
/// Deliberately narrow: textDelta / tool-call floods stay inside the owning device's own
/// turn stream; other devices only learn that *something* happened.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EventPing {
    pub kind: &'static str,
    pub turn_id: Option<String>,
    pub session_id: Option<String>,
}

impl EventPing {
    pub(crate) fn of(notification: &ServerNotification) -> Option<EventPing> {
        let ping = match notification {
            ServerNotification::TurnStarted(n) => EventPing {
                kind: "turn_started",
                turn_id: Some(n.turn_id.clone()),
                session_id: Some(n.session_id.clone()),
            },
            ServerNotification::TurnCompleted(n) => EventPing {
                kind: "turn_completed",
                turn_id: Some(n.turn_id.clone()),
                session_id: None,
            },
            ServerNotification::ApprovalRequested(n) => EventPing {
                kind: "approval",
                turn_id: Some(n.turn_id.clone()),
                session_id: None,
            },
            ServerNotification::GraphStarted(_) => EventPing {
                kind: "graph",
                turn_id: None,
                session_id: None,
            },
            ServerNotification::ConsolidationCompleted(_) => EventPing {
                kind: "memory",
                turn_id: None,
                session_id: None,
            },
            ServerNotification::Error(_) => EventPing {
                kind: "error",
                turn_id: None,
                session_id: None,
            },
            // The long-task engine's knocks: a boundary landed, a crashed turn is being
            // continued, a checkpoint was closed without resuming.
            ServerNotification::TurnCheckpointed(n) => EventPing {
                kind: "task_progress",
                turn_id: Some(n.turn_id.clone()),
                session_id: Some(n.session_id.clone()),
            },
            ServerNotification::TurnResumed(n) => EventPing {
                kind: "task_resumed",
                turn_id: Some(n.turn_id.clone()),
                session_id: Some(n.session_id.clone()),
            },
            ServerNotification::TurnSuspended(n) => EventPing {
                kind: "task_suspended",
                turn_id: Some(n.turn_id.clone()),
                session_id: Some(n.session_id.clone()),
            },
            // A long task changed state: created / running / paused / done. Same knock — the
            // receiver pulls the details (the task card is one /api/data away).
            ServerNotification::TaskUpdated(n) => EventPing {
                kind: "task",
                turn_id: None,
                session_id: Some(n.session_id.clone()),
            },
            // Flow noise (text deltas, tool calls, gates, retries) — knock once per
            // meaningful change, not once per token.
            _ => return None,
        };
        Some(ping)
    }
}

/// `GET /api/events` — one SSE channel of pings for this session's app-server.
///
/// The connection holds the app-server Arc, so a phone keeping this open pins its
/// instance in the hub pool (idle eviction skips pinned instances — by design: the
/// "idle" phone is the cheapest warm start there is).
pub async fn events(Extension(app): Extension<Arc<AppServer>>) -> Response {
    use futures_util::StreamExt;

    let rx = app.subscribe();
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(notification) => {
                    if let Some(ping) = EventPing::of(&notification) {
                        return Some((ping, rx));
                    }
                    // Flow noise: keep waiting for the next meaningful one.
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
    .map(|ping| {
        Ok::<_, std::convert::Infallible>(
            Event::default().data(serde_json::to_string(&ping).expect("ping 装得进 JSON")),
        )
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// `GET /api/data` — everything the first screen needs, fetched in one go.
///
/// One endpoint instead of four concurrent frontend requests: the app-server handles one
/// request at a time (over stdio it is just a loop), so four requests queue there anyway.
/// Composing one also guarantees "these numbers are from the same moment".
pub async fn data(
    Extension(app): Extension<Arc<AppServer>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    match collect(&app, params.get("sessionId").map(String::as_str)).await {
        Ok(data) => Json(data).into_response(),
        Err(error) => failed(StatusCode::BAD_GATEWAY, error),
    }
}

async fn collect(app: &AppServer, session: Option<&str>) -> Result<DashboardData, ErrorObject> {
    let config: ConfigReadResponse = app.request(methods::CONFIG_READ, json!({})).await?;
    let sessions: SessionListResponse = app
        .request(methods::SESSION_LIST, json!({ "limit": SESSION_LIMIT }))
        .await?;
    let facts: MemoryListResponse = app
        .request(methods::MEMORY_LIST, json!({ "limit": FACT_LIMIT }))
        .await?;
    let episodes: MemoryListEpisodesResponse = app
        .request(
            methods::MEMORY_LIST_EPISODES,
            json!({ "limit": EPISODE_LIMIT }),
        )
        .await?;

    // Goal and todo are **per viewed session**: only fetched when the page names one.
    // Two read-only RPC methods — ops is a client of app-server, the same world as
    // every other client; the stdio queue is the honest path, even for reads.
    let (goal, todos, task) = match session {
        Some(id) => {
            let goal: GoalSetResponse = app
                .request(methods::GOAL_VIEW, json!({ "sessionId": id }))
                .await?;
            let todos: Option<TodoListView> = app
                .request(methods::TODO_VIEW, json!({ "sessionId": id }))
                .await?;
            // The long task behind that goal (0.11.0): status + round account + the pause switch.
            let task: TaskViewResponse = app
                .request(methods::TASK_VIEW, json!({ "sessionId": id }))
                .await?;
            (Some(goal), todos, task.task)
        }
        None => (None, None, None),
    };

    Ok(DashboardData {
        generated_at: Local::now().to_rfc3339(),
        config: config.config,
        sessions: sessions.data,
        facts: facts.data,
        episodes: episodes.data,
        goal,
        todos,
        task,
    })
}

/// `GET /api/session?sessionId=…&cursor=…&limit=…` — what one session said.
///
/// Why not in `/api/data`: that screen is an **overview**, fully refreshed every 15 seconds,
/// answering "what is the state now". This is **history** — per session, paged upward;
/// paging should not re-fetch config and memory along with it.
///
/// Query params reuse the protocol's `SessionMessagesParams` (**no second
/// shape**), so the URL carries exactly `sessionId` / `cursor` / `limit`;
/// which one is required is decided by that type, not rewritten here.
pub async fn session(
    Extension(app): Extension<Arc<AppServer>>,
    Query(params): Query<SessionMessagesParams>,
) -> Response {
    let payload = serde_json::to_value(&params).expect("SessionMessagesParams 一定能序列化");
    match app
        .request::<SessionMessagesResponse>(methods::SESSION_MESSAGES, payload)
        .await
    {
        Ok(page) => Json(page).into_response(),
        Err(error) => failed(StatusCode::BAD_GATEWAY, error),
    }
}

/// `POST /api/approval` — answer one "should this run".
/// `POST /api/goal` — set or clear a goal (missing/empty `condition` = clear).
///
/// Same path as turn: the app-server is a subprocess, `goal/set` one of its methods.
/// **The dashboard is not the only entry point**: the terminal's `/goal` is the same thing.
pub async fn goal(
    Extension(app): Extension<Arc<AppServer>>,
    Json(params): Json<GoalSetParams>,
) -> Response {
    let payload = serde_json::to_value(&params).expect("GoalSetParams 一定能序列化");
    match app
        .request::<GoalSetResponse>(methods::GOAL_SET, payload)
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// `GET /api/tasks` — the long-task panel: every task, newest first.
pub async fn tasks(Extension(app): Extension<Arc<AppServer>>) -> Response {
    match app
        .request::<TaskListResponse>(methods::TASK_LIST, json!({}))
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::BAD_GATEWAY, error),
    }
}

/// `POST /api/task` — create / suspend / resume / cancel, one door for the human's switches.
///
/// The body carries `action` plus that action's params; ops only translates — every rule
/// (one active task per session, paused checkpoints stay resumable) lives in app-server.
pub async fn task(
    Extension(app): Extension<Arc<AppServer>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let action = body
        .get("action")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    // Translate the action into (method, payload); the request happens once, below.
    let translated: Result<(&str, serde_json::Value), ErrorObject> = match action.as_str() {
        "create" => serde_json::from_value::<TaskCreateParams>(body.clone())
            .map(|params| {
                (
                    methods::TASK_CREATE,
                    serde_json::to_value(params).expect("可序列化"),
                )
            })
            .map_err(|e| ErrorObject {
                code: codes::INVALID_PARAMS,
                message: format!("建任务的参数不对：{e}"),
                data: None,
            }),
        "suspend" | "resume" | "cancel" => serde_json::from_value::<TaskActionParams>(body.clone())
            .map(|params| {
                let method = match action.as_str() {
                    "suspend" => methods::TASK_SUSPEND,
                    "resume" => methods::TASK_RESUME,
                    _ => methods::TASK_CANCEL,
                };
                (method, serde_json::to_value(params).expect("可序列化"))
            })
            .map_err(|e| ErrorObject {
                code: codes::INVALID_PARAMS,
                message: format!("任务动作的参数不对：{e}"),
                data: None,
            }),
        other => Err(ErrorObject {
            code: codes::INVALID_PARAMS,
            message: format!("不认识的任务动作：{other}（create/suspend/resume/cancel）"),
            data: None,
        }),
    };
    let result = match translated {
        Ok((method, payload)) => app.request::<TaskEnvelope>(method, payload).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(envelope) => Json(envelope).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

///
/// The payload is the protocol's `ApprovalRespondParams`. **Not SSE**: one question, one
/// answer, then gone. Too late → `accepted: false` (the turn stopped waiting long ago);
/// the frontend knows its click went unheard instead of assuming the approval took effect.
/// Configured model profiles + the current one (**no keys**, only a "where is the key" ptr).
///
/// Same discipline as `/api/goal`: these are **human-only** settings — the model has no
/// `model/*` tools and cannot reach the keys.
/// Usage ledger (migration 0010): per-model aggregates + totals. Bookkeeping, not enforcing.
pub async fn usage(Extension(app): Extension<Arc<AppServer>>) -> Response {
    match app
        .request::<UsageSummaryResponse>(methods::USAGE_SUMMARY, json!({}))
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// Approval audit (migration 0011): the last 100 "asked before running", newest first.
pub async fn approvals(Extension(app): Extension<Arc<AppServer>>) -> Response {
    match app
        .request::<ApprovalLogResponse>(methods::APPROVAL_LOG, json!({}))
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// Schedule status snapshot (migration 0012): per-task last_fired/claimed/catch_up/delivery.
pub async fn schedules(Extension(app): Extension<Arc<AppServer>>) -> Response {
    match app
        .request::<ScheduleListResponse>(methods::SCHEDULE_LIST, json!({}))
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// Cap for a single uploaded file: 20MB.
const UPLOAD_MAX_BYTES: usize = 20 * 1024 * 1024;

/// Extension whitelist: 1-8 alphanumeric chars; no double extensions or weird chars.
fn safe_extension(name: &str) -> Option<String> {
    let ext = std::path::Path::new(name).extension()?.to_str()?;
    let ok = !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric());
    ok.then(|| ext.to_ascii_lowercase())
}

/// `POST /api/upload?name=<original filename>` — body is the raw file bytes.
///
/// octet-stream instead of multipart: a single file needs no multipart envelope, and it
/// avoids the parser, a security minefield (zero features, zero transitive deps). The
/// on-disk name is **generated entirely server-side** (spill's discipline: user input
/// never enters a path); only the extension survives, through the whitelist. Returns a
/// home-relative path — the frontend injects it; the model reads it via read_file itself.
pub async fn upload(
    Extension(app): Extension<Arc<AppServer>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > UPLOAD_MAX_BYTES {
        return failed(
            StatusCode::PAYLOAD_TOO_LARGE,
            ErrorObject {
                code: codes::INVALID_PARAMS,
                message: format!("文件超过 20MB 上限（{} 字节）", body.len()),
                data: None,
            },
        );
    }
    if body.is_empty() {
        return failed(
            StatusCode::BAD_REQUEST,
            ErrorObject {
                code: codes::INVALID_PARAMS,
                message: "空文件不收。".into(),
                data: None,
            },
        );
    }
    let ext = params
        .get("name")
        .and_then(|n| safe_extension(n))
        .unwrap_or_else(|| "bin".to_string());
    // Home comes from the protocol (AppServer is a subprocess handle, no local field) —
    // upload is rare; an extra round trip keeps ops "protocol-only" discipline intact.
    let config: ConfigReadResponse = match app.request(methods::CONFIG_READ, json!({})).await {
        Ok(config) => config,
        Err(error) => return failed(StatusCode::BAD_GATEWAY, error),
    };
    let home = std::path::PathBuf::from(config.config.home);
    let now = Local::now();
    let dir = home.join("uploads").join(now.format("%Y%m%d").to_string());
    if let Err(why) = tokio::fs::create_dir_all(&dir).await {
        return failed(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorObject {
                code: codes::INTERNAL_ERROR,
                message: format!("建目录失败：{why}"),
                data: None,
            },
        );
    }
    // Only server-generated parts: timestamp + nanosecond suffix — no collisions, no overwrite.
    let name = format!(
        "file-{}-{:x}.{ext}",
        now.format("%H%M%S"),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    );
    let full = dir.join(&name);
    if let Err(why) = tokio::fs::write(&full, &body).await {
        return failed(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorObject {
                code: codes::INTERNAL_ERROR,
                message: format!("写文件失败：{why}"),
                data: None,
            },
        );
    }
    let date = now.format("%Y%m%d");
    Json(json!({ "path": format!("uploads/{date}/{name}") })).into_response()
}

/// The directory-browsing response. An ops-local UI view (as with /api/upload's response):
/// not in the protocol — neither the SDK nor the terminal needs "browse server directories".
#[derive(serde::Serialize)]
pub struct FsListing {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<FsEntry>,
}

#[derive(serde::Serialize)]
pub struct FsEntry {
    pub name: String,
    pub path: String,
}

/// List the **subdirectories** of a directory (read-only, dirs only — picking a workdir has
/// no use for files). A pure function so it can be unit-tested without booting an app-server.
pub fn list_dirs(raw: &str, include_hidden: bool) -> Result<FsListing, String> {
    let path = std::path::PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(format!("'{raw}' 必须是绝对路径"));
    }
    if !path.is_dir() {
        return Err(format!("'{raw}' 不存在或不是目录"));
    }
    let mut entries: Vec<FsEntry> = std::fs::read_dir(&path)
        .map_err(|why| format!("读不动 {raw}：{why}"))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let is_dir = entry.file_type().ok()?.is_dir();
            if !is_dir {
                return None;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            // Dot-prefixed entries hidden by default, shown only when the switch is on;
            // build noise (node_modules/target) hidden regardless — stepping in is pointless.
            if (name.starts_with('.') && !include_hidden)
                || name == "node_modules"
                || name == "target"
            {
                return None;
            }
            Some(FsEntry {
                path: entry.path().to_string_lossy().to_string(),
                name,
            })
        })
        .collect();
    entries.sort_by_key(|e| e.name.to_lowercase());
    Ok(FsListing {
        path: path.to_string_lossy().to_string(),
        parent: path
            .parent()
            .filter(|p| p.as_os_str() != path.as_os_str())
            .map(|p| p.to_string_lossy().to_string()),
        entries,
    })
}

/// Where browsing starts: the user's home directory, falling back to process cwd.
fn default_browse_root() -> String {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .or_else(|_| std::env::current_dir().map(|p| p.to_string_lossy().to_string()))
        .unwrap_or_else(|_| "/".to_string())
}

/// `GET /api/fs?path=<dir>` — server-side directory browsing (dashboard picks a workdir).
///
/// Why server-side: the browser folder picker **cannot get absolute paths** (the File
/// System Access API gives handles, not paths), while a workdir needs exactly an absolute
/// path — so "pick a folder like a desktop app" falls to the filesystem-aware side (the
/// Jupyter / VS Code server approach). Read-only, directories only.
pub async fn fs_browse(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let raw: String = params
        .get("path")
        .map(String::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(default_browse_root);
    let include_hidden = params
        .get("hidden")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    match list_dirs(&raw, include_hidden) {
        Ok(listing) => Json(listing).into_response(),
        Err(why) => failed(
            StatusCode::BAD_REQUEST,
            ErrorObject {
                code: codes::INVALID_PARAMS,
                message: why,
                data: None,
            },
        ),
    }
}

/// Create a subdirectory under parent, returning its absolute path. Pure fn for unit tests.
///
/// Same name discipline as upload's extension whitelist: **user input never
/// enters the path shape** — no separators, no `..`, no dot prefix (a created
/// directory should not be invisible by default), capped at 64 chars.
pub fn create_subdir(parent_raw: &str, name: &str) -> Result<String, String> {
    let parent = std::path::PathBuf::from(parent_raw);
    if !parent.is_absolute() {
        return Err(format!("父目录 '{parent_raw}' 必须是绝对路径"));
    }
    if !parent.is_dir() {
        return Err(format!("父目录 '{parent_raw}' 不存在或不是目录"));
    }
    let name = name.trim();
    let bad = |why: &str| format!("目录名 '{name}' 不合格：{why}");
    if name.is_empty() || name.len() > 64 {
        return Err(bad("1-64 个字符"));
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") || name.starts_with('.') {
        return Err(bad("不能包含路径形状（分隔符、..、点开头）"));
    }
    let target = parent.join(name);
    if target.exists() {
        return Err(format!("'{name}' 在 {parent_raw} 下已经存在"));
    }
    std::fs::create_dir(&target).map_err(|why| format!("建不动：{why}"))?;
    Ok(target.to_string_lossy().to_string())
}

/// `POST /api/fs/mkdir?path=<parent>&name=<name>` — the picker's "new folder" button; the
/// only write action besides read-only browsing. Names go through the whitelist.
pub async fn fs_mkdir(Query(params): Query<std::collections::HashMap<String, String>>) -> Response {
    let parent = params.get("path").cloned().unwrap_or_default();
    let name = params.get("name").cloned().unwrap_or_default();
    match create_subdir(&parent, &name) {
        Ok(created) => Json(json!({ "path": created })).into_response(),
        Err(why) => failed(
            StatusCode::BAD_REQUEST,
            ErrorObject {
                code: codes::INVALID_PARAMS,
                message: why,
                data: None,
            },
        ),
    }
}

/// The "recents" response (ops-local view, same treatment as /api/fs).
fn recents_response(home: &std::path::Path) -> Response {
    Json(json!({
        "paths": joyczl_config::workspaces::load_workspaces(home).paths,
    }))
    .into_response()
}

/// `GET /api/fs/recents` — recently used directories (quick entry for picking a workdir).
pub async fn fs_recents(Extension(app): Extension<Arc<AppServer>>) -> Response {
    let config: ConfigReadResponse = match app.request(methods::CONFIG_READ, json!({})).await {
        Ok(config) => config,
        Err(error) => return failed(StatusCode::BAD_GATEWAY, error),
    };
    recents_response(std::path::Path::new(&config.config.home))
}

/// `POST /api/fs/recents?path=<dir>` — record a picker choice (recorded even before a turn
/// runs: the user's intent is already expressed here).
pub async fn fs_recents_record(
    Extension(app): Extension<Arc<AppServer>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let config: ConfigReadResponse = match app.request(methods::CONFIG_READ, json!({})).await {
        Ok(config) => config,
        Err(error) => return failed(StatusCode::BAD_GATEWAY, error),
    };
    let home = std::path::PathBuf::from(config.config.home);
    if let Some(path) = params
        .get("path")
        .map(String::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        joyczl_config::workspaces::record_workspace(&home, path);
    }
    recents_response(&home)
}

pub async fn model_profiles(Extension(app): Extension<Arc<AppServer>>) -> Response {
    match app
        .request::<ModelProfilesResponse>(methods::MODEL_PROFILES, json!({}))
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// Add/update a profile (and make it current), or just switch via `id` + `makeActive`.
pub async fn model_save(
    Extension(app): Extension<Arc<AppServer>>,
    Json(params): Json<ModelSaveParams>,
) -> Response {
    let payload = serde_json::to_value(&params).expect("ModelSaveParams 一定能序列化");
    match app
        .request::<ModelSaveResponse>(methods::MODEL_SAVE, payload)
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// Remove a profile (along with its credentials).
pub async fn model_remove(
    Extension(app): Extension<Arc<AppServer>>,
    Json(params): Json<ModelRemoveParams>,
) -> Response {
    let payload = serde_json::to_value(&params).expect("ModelRemoveParams 一定能序列化");
    match app
        .request::<ModelRemoveResponse>(methods::MODEL_REMOVE, payload)
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

pub async fn approval(
    Extension(app): Extension<Arc<AppServer>>,
    Json(params): Json<ApprovalRespondParams>,
) -> Response {
    let payload = serde_json::to_value(&params).expect("ApprovalRespondParams 一定能序列化");
    // Same path as turn: the app-server is a subprocess, answering is one of its methods.
    match app
        .request::<ApprovalRespondResponse>(methods::APPROVAL_RESPOND, payload)
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// `POST /api/interrupt` — 停一个在跑的 turn。
///
/// 载荷就是协议的 `TurnInterruptParams`（**没有第二种形状**）；turnId 由前端从
/// `turnStarted` 事件里拿到——它是这一轮的唯一句柄。太晚（turn 已结束）→
/// `interrupted: false`，前端如实显示，不假装停掉了什么。
pub async fn interrupt(
    Extension(app): Extension<Arc<AppServer>>,
    Json(params): Json<TurnInterruptParams>,
) -> Response {
    let payload = serde_json::to_value(&params).expect("TurnInterruptParams 一定能序列化");
    match app
        .request::<TurnInterruptResponse>(methods::TURN_INTERRUPT, payload)
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// `POST /api/turn` — say a sentence, watch what comes back.
///
/// The payload is the protocol's `TurnStartParams` (**no second shape**); the
/// response is an SSE stream: `data:` followed by the JSON of a
/// `ServerNotification`, the same thing received over stdio.
///
/// Why SSE and not WebSocket: this path has one direction only, "server → browser", and
/// sending a message is already an ordinary POST. Duplex would drag in useless state
/// management, and the browser-native EventSource only does GET — it cannot send this POST.
pub async fn turn(
    Extension(app): Extension<Arc<AppServer>>,
    Json(params): Json<TurnStartParams>,
) -> Response {
    if params.message.trim().is_empty() {
        return failed(
            StatusCode::BAD_REQUEST,
            ErrorObject {
                code: codes::INVALID_PARAMS,
                message: "消息是空的".to_string(),
                data: None,
            },
        );
    }

    let session_id = params
        .session_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "default".to_string());

    let events = app.subscribe();
    let (tx, rx) = mpsc::channel::<ServerNotification>(TURN_BUFFER);

    let payload = serde_json::to_value(&params).expect("TurnStartParams 一定能序列化");
    tokio::spawn(async move {
        forward(&app, payload, session_id, tx, events).await;
    });

    Sse::new(bytes_stream(rx))
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Forward this turn's notifications to the browser one by one (subscriptions + response).
///
/// The request's response (the `turn/start` return) is usually **ignored by everyone** —
/// `turnCompleted` already carries back reply, usage and telemetry. But the request may
/// fail outright (bad model config or params); then it is the only word, and must be shown.
async fn forward(
    app: &AppServer,
    payload: serde_json::Value,
    session_id: String,
    tx: mpsc::Sender<ServerNotification>,
    mut events: broadcast::Receiver<ServerNotification>,
) {
    let request = app.request::<TurnStartResponse>(methods::TURN_START, payload);
    tokio::pin!(request);
    let mut answered = false;
    let mut turn_id: Option<String> = None;

    loop {
        tokio::select! {
            result = &mut request, if !answered => {
                answered = true;
                if let Err(error) = result {
                    // Once handed to the page, we're done: this turn never ran.
                    let _ = tx.send(crate::as_notification(error)).await;
                    break;
                }
            }
            received = events.recv() => {
                let notification = match received {
                    Ok(notification) => notification,
                    // Lapped: intermediate notifications fell behind. Received content is
                    // kept, but it must be said, or the page shows "a chunk mysteriously gone".
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        eprintln!("(joy) 通知推得太快，这一轮丢了 {missed} 条");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };

                let Some(id) = belongs(&notification, &session_id, turn_id.as_deref()) else {
                    // Someone else's turn (another session started on the gateway side). The
                    // broadcast reaches all subscribers; filtering can only happen here.
                    continue;
                };
                turn_id = Some(id);

                let last = matches!(notification, ServerNotification::TurnCompleted(_));
                if tx.send(notification).await.is_err() {
                    // The browser closed the tab: stop pushing; the turn finishes on its own.
                    break;
                }
                if last {
                    break;
                }
            }
        }
    }
}

/// Does this notification belong to this turn? If so, return its turnId.
///
/// The first frame, `turnStarted`, can only be recognized by sessionId (no turnId yet);
/// every later frame carries a turnId — id matching avoids mixing in "another session that
/// happens to be running". `consolidationCompleted` and `graph*` have no turnId; they are
/// forwarded only after this turn has already been claimed.
fn belongs(
    notification: &ServerNotification,
    session_id: &str,
    known: Option<&str>,
) -> Option<String> {
    use ServerNotification as N;
    match notification {
        N::TurnStarted(started) if started.session_id == session_id => {
            Some(started.turn_id.clone())
        }
        _ if known.is_none() => None,
        other => match turn_id_of(other) {
            Some(id) if Some(id) == known => Some(id.to_string()),
            Some(_) => None,
            None => known.map(str::to_string),
        },
    }
}

/// The turnId carried in a notification. Variants without that field return None (see `belongs`).
fn turn_id_of(notification: &ServerNotification) -> Option<&str> {
    use ServerNotification as N;
    match notification {
        N::TurnStarted(n) => Some(&n.turn_id),
        N::TextDelta(n) => Some(&n.turn_id),
        N::GateDecided(n) => Some(&n.turn_id),
        N::Retry(n) => Some(&n.turn_id),
        N::ApprovalRequested(n) => Some(&n.turn_id),
        N::GoalRound(n) => Some(&n.turn_id),
        N::ToolStarted(n) => Some(&n.turn_id),
        N::ToolCompleted(n) => Some(&n.turn_id),
        N::TurnCompleted(n) => Some(&n.turn_id),
        N::TurnCheckpointed(n) => Some(&n.turn_id),
        N::TurnResumed(n) => Some(&n.turn_id),
        N::TurnSuspended(n) => Some(&n.turn_id),
        N::TaskUpdated(_)
        | N::ConsolidationCompleted(_)
        | N::GraphStarted(_)
        | N::GraphNodeStarted(_)
        | N::GraphNodeEnded(_)
        | N::GraphEnded(_)
        | N::Error(_) => None,
    }
}

/// Notification channel → SSE frames.
///
/// Each frame is the JSON of one notification (`data: {...}`); the frontend `JSON.parse`s
/// it and dispatches on the `type` field — an exact match for `ServerNotification`'s
/// discriminant, so the frontend needs no second schema.
fn bytes_stream(
    rx: mpsc::Receiver<ServerNotification>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    stream::unfold(rx, |mut rx| async move {
        let notification = rx.recv().await?;
        let json = serde_json::to_string(&notification).expect("通知一定能序列化");
        Some((Ok(Event::default().data(json)), rx))
    })
}

/// On error, return the protocol's own `ErrorObject`, untouched.
///
/// The status code is not 200: on the `fetch` side "this trip failed" is obvious at a
/// glance, while the body is still the familiar `{code, message}` — the page can just show
/// the message; what app-server wrote was meant for users in the first place.
fn failed(status: StatusCode, error: ErrorObject) -> Response {
    (status, Json(error)).into_response()
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod api_tests;
