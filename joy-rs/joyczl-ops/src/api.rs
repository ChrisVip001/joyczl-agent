//! The dashboard's HTTP endpoints.
//!
//! Only a handful, because that is all a dashboard does: **see the picture**, **dig
//! history**, **talk**, and **manage what the human owns** (the capability center's
//! skills and MCP connectors). Everything is translated from app-server reads and
//! `turn/start`; no business logic is added here — new business belongs on the
//! protocol side, and the capability center's domain lives in `capabilities.rs`.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Local;
use futures_util::stream::{self, Stream};
use joyczl_mcp::config::ServerSpec;
use joyczl_protocol::{
    codes, methods, ApprovalLogResponse, ApprovalRespondParams, ApprovalRespondResponse,
    ConfigReadResponse, ConnectorStatus, DashboardData, ErrorObject, InboxListView,
    InboxSummarizeParams, InboxSummarizeResponse, MemoryListEpisodesResponse, MemoryListResponse,
    MessageRole, ModelProfilesResponse, ModelRemoveParams, ModelRemoveResponse,
    ModelRoutesResponse, ModelRoutesSaveParams, ModelSaveParams, ModelSaveResponse, RunResponse,
    ScheduleListResponse, ServerNotification, SessionForkParams, SessionForkResponse,
    SessionGetParams, SessionGetResponse, SessionListResponse, SessionMessagesParams,
    SessionMessagesResponse, SessionNewParams, SessionNewResponse, SessionUpdateParams,
    SessionUpdateResponse, TaskActionParams, TaskArtifactsResponse, TaskCreateParams, TaskEnvelope,
    TaskListResponse, TaskViewResponse, TodoListView, TurnFollowUpParams, TurnFollowUpResponse,
    TurnInterruptParams, TurnInterruptResponse, TurnListParams, TurnListResponse, TurnStartParams,
    TurnStartResponse, TurnSteerParams, TurnSteerResponse, UsageSummaryResponse,
};
use serde_json::json;
use tokio::sync::{broadcast, mpsc};

use crate::{capabilities, webhooks, AppServer};

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
            // receiver pulls the details (the task card is one /api/data away). A task with no
            // session carries no session in the ping (0.14: tasks stand on their own).
            ServerNotification::TaskUpdated(n) => EventPing {
                kind: "task",
                turn_id: None,
                session_id: n.session_id.clone(),
            },
            // Inbound: new mail waiting for a human. Not tied to a session — it is the
            // inbox's news, and the panel lives on the overview.
            ServerNotification::InboxReceived(_) => EventPing {
                kind: "inbox",
                turn_id: None,
                session_id: None,
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
            Event::default().data(serde_json::to_string(&ping).expect("ping serialises into JSON")),
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
    // Inbound mail: the panel's list. Empty when no connector is configured — ops does not
    // decide that, app-server's inbox table does.
    let inbox: InboxListView = app
        .request(methods::INBOX_LIST, json!({}))
        .await
        .unwrap_or_else(|error| {
            eprintln!(
                "(joy) could not read the inbox (the panel will show it empty): {}",
                error.message
            );
            InboxListView {
                items: Vec::new(),
                unseen: 0,
            }
        });
    // Connectors: not part of the app-server protocol — the file is the truth and ops
    // sits on the same machine. A broken file is reported, never rendered as "none".
    let (connectors, connectors_error) = connector_status(app);
    let (todos, task, task_artifacts) = match session {
        Some(id) => {
            let todos: Option<TodoListView> = app
                .request(methods::TODO_VIEW, json!({ "sessionId": id }))
                .await?;
            // The session's task (0.11.0): status + round account + the pause switch. Since 0.17
            // this is also where the goal lives — `goalCondition` is the condition `/goal` wrote.
            // Since 0.14 it also carries acceptance verdicts and spend, so the card can show
            // **why** it is not done instead of just that it is not.
            let task: TaskViewResponse = app
                .request(methods::TASK_VIEW, json!({ "sessionId": id }))
                .await?;
            // Deliverables are a second read: the list can be long, and the card shows the top
            // few. A failure here is "no files shown", never "no files exist".
            let task_artifacts = match &task.task {
                Some(row) => app
                    .request::<TaskArtifactsResponse>(
                        methods::TASK_ARTIFACTS,
                        json!({ "taskId": row.id }),
                    )
                    .await
                    .map(|response| response.artifacts)
                    .unwrap_or_else(|error| {
                        eprintln!("(joy) could not read the task deliverables (the panel shows them empty): {}", error.message);
                        Vec::new()
                    }),
                None => Vec::new(),
            };
            (todos, task.task, task_artifacts)
        }
        None => (None, None, Vec::new()),
    };

    Ok(DashboardData {
        generated_at: Local::now().to_rfc3339(),
        config: config.config,
        sessions: sessions.data,
        facts: facts.data,
        episodes: episodes.data,
        todos,
        task,
        task_artifacts,
        inbox,
        connectors,
        connectors_error,
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
    let payload = serde_json::to_value(&params).expect("SessionMessagesParams always serialises");
    match app
        .request::<SessionMessagesResponse>(methods::SESSION_MESSAGES, payload)
        .await
    {
        Ok(page) => Json(page).into_response(),
        Err(error) => failed(StatusCode::BAD_GATEWAY, error),
    }
}

/// `GET /api/session/meta?sessionId=` — one session's record (name, working directory, count).
///
/// Separate from `/api/session` on purpose: that one pages *messages*, and answering "which
/// folder does this conversation write into" must not drag a conversation out of the database.
pub async fn session_meta(
    Extension(app): Extension<Arc<AppServer>>,
    Query(params): Query<SessionGetParams>,
) -> Response {
    let payload = serde_json::to_value(&params).expect("SessionGetParams always serialises");
    match app
        .request::<SessionGetResponse>(methods::SESSION_GET, payload)
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// `POST /api/session/new` — create a session that exists before its first message (0.15).
///
/// The id is allocated by app-server, not by the browser: two tabs (or two devices) generating
/// ids locally is how conversations end up sharing a row. A name and a working directory can be
/// given at creation, so an empty session is already meaningful in the list.
pub async fn session_new(
    Extension(app): Extension<Arc<AppServer>>,
    Json(params): Json<SessionNewParams>,
) -> Response {
    let payload = serde_json::to_value(&params).expect("SessionNewParams always serialises");
    match app
        .request::<SessionNewResponse>(methods::SESSION_NEW, payload)
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// `POST /api/session/update` — rename, or rebind the working directory ("" clears).
pub async fn session_update(
    Extension(app): Extension<Arc<AppServer>>,
    Json(params): Json<SessionUpdateParams>,
) -> Response {
    let payload = serde_json::to_value(&params).expect("SessionUpdateParams always serialises");
    match app
        .request::<SessionUpdateResponse>(methods::SESSION_UPDATE, payload)
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// `POST /api/session/fork` — branch a conversation: copy the last few exchanges into a new
/// session and leave the original running untouched.
pub async fn session_fork(
    Extension(app): Extension<Arc<AppServer>>,
    Json(params): Json<SessionForkParams>,
) -> Response {
    let payload = serde_json::to_value(&params).expect("SessionForkParams always serialises");
    match app
        .request::<SessionForkResponse>(methods::SESSION_FORK, payload)
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// `GET /api/turns?sessionId=…` — which turns are running right now.
///
/// The page cannot answer this from `busy` alone: another device may be the one running the
/// turn, and after a reload the tab has no memory of it. Absent `sessionId` = the whole home.
pub async fn turns(
    Extension(app): Extension<Arc<AppServer>>,
    Query(params): Query<TurnListParams>,
) -> Response {
    let payload = serde_json::to_value(&params).expect("TurnListParams always serialises");
    match app
        .request::<TurnListResponse>(methods::TURN_LIST, payload)
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::BAD_GATEWAY, error),
    }
}

/// `POST /api/turn/steer` — add a sentence to the turn already running in this conversation.
///
/// Not a second `turn/start`: that one is refused while a turn runs, and cancelling first would
/// throw away tool work that already happened. The addition is accepted now and folded in at the
/// loop's next iteration boundary.
pub async fn steer(
    Extension(app): Extension<Arc<AppServer>>,
    Json(params): Json<TurnSteerParams>,
) -> Response {
    let payload = serde_json::to_value(&params).expect("TurnSteerParams always serialises");
    match app
        .request::<TurnSteerResponse>(methods::TURN_STEER, payload)
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// `POST /api/turn/follow-up` — queue a message for **after** the turn in flight.
///
/// The difference from `steer` is what the person meant: steer is "and also, while you are at
/// it"; this is "when you are done". It becomes its own turn, so it keeps its own rounds and
/// budget, and it runs even if this page is closed.
pub async fn follow_up(
    Extension(app): Extension<Arc<AppServer>>,
    Json(params): Json<TurnFollowUpParams>,
) -> Response {
    let payload = serde_json::to_value(&params).expect("TurnFollowUpParams always serialises");
    match app
        .request::<TurnFollowUpResponse>(methods::TURN_FOLLOW_UP, payload)
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// `POST /api/approval` — answer one "should this run".
/// What the connector panel shows: one row per configured connector, or the reason the
/// file could not be read. The allowlist size is the visible width of the door.
fn connector_status(app: &AppServer) -> (Vec<ConnectorStatus>, Option<String>) {
    match joyczl_connector::config::load(app.home()) {
        Ok(config) => {
            let mut rows = Vec::new();
            if let Some(email) = config.email {
                rows.push(ConnectorStatus {
                    id: "email".to_string(),
                    live: email.live,
                    allow_recipients: email.allow_recipients.len(),
                });
            }
            if let Some(feishu) = config.feishu {
                rows.push(ConnectorStatus {
                    id: "feishu".to_string(),
                    live: feishu.live,
                    allow_recipients: feishu.allow_recipients.len(),
                });
            }
            (rows, None)
        }
        Err(e) => (Vec::new(), Some(e.to_string())),
    }
}

/// `POST /api/inbox` — the human's "handle these": one turn over the unseen mail.
///
/// Body: `{ "sessionId": "inbox" }` (optional). No model configured → 200 with the
/// app-server's own honest error, same convention as the task switches.
pub async fn inbox(
    Extension(app): Extension<Arc<AppServer>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let params: InboxSummarizeParams = match serde_json::from_value(body) {
        Ok(params) => params,
        Err(e) => {
            return failed(
                StatusCode::BAD_REQUEST,
                ErrorObject {
                    code: codes::INVALID_PARAMS,
                    message: format!("bad inbox parameters: {e}"),
                    data: None,
                },
            )
        }
    };
    match app
        .request::<InboxSummarizeResponse>(
            methods::INBOX_SUMMARIZE,
            serde_json::to_value(params).expect("serialisable"),
        )
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
    if matches!(action.as_str(), "operations" | "reconcile") {
        let translated = if action == "operations" {
            serde_json::from_value::<joyczl_protocol::TaskGetParams>(body).map(|params| {
                (
                    methods::TASK_OPERATIONS,
                    serde_json::to_value(params).expect("serialisable"),
                )
            })
        } else {
            serde_json::from_value::<joyczl_protocol::TaskReconcileParams>(body).map(|params| {
                (
                    methods::TASK_RECONCILE,
                    serde_json::to_value(params).expect("serialisable"),
                )
            })
        };
        let result = match translated {
            Ok((method, payload)) => {
                app.request::<joyczl_protocol::TaskOperationsResponse>(method, payload)
                    .await
            }
            Err(error) => Err(ErrorObject {
                code: codes::INVALID_PARAMS,
                message: format!("Bad operation parameters: {error}"),
                data: None,
            }),
        };
        return match result {
            Ok(response) => Json(response).into_response(),
            Err(error) => failed(StatusCode::OK, error),
        };
    }
    if action == "signal" {
        let result = match serde_json::from_value::<joyczl_protocol::TaskSignalParams>(body) {
            Ok(params) => {
                app.request::<joyczl_protocol::TaskSignalResponse>(
                    methods::TASK_SIGNAL,
                    serde_json::to_value(params).expect("serialisable"),
                )
                .await
            }
            Err(error) => Err(ErrorObject {
                code: codes::INVALID_PARAMS,
                message: format!("Bad task-input parameters: {error}"),
                data: None,
            }),
        };
        return match result {
            Ok(response) => Json(response).into_response(),
            Err(error) => failed(StatusCode::OK, error),
        };
    }
    // Translate the action into (method, payload); the request happens once, below.
    let translated: Result<(&str, serde_json::Value), ErrorObject> = match action.as_str() {
        "wait" => serde_json::from_value::<joyczl_protocol::TaskWaitParams>(body.clone())
            .map(|params| {
                (
                    methods::TASK_WAIT,
                    serde_json::to_value(params).expect("serialisable"),
                )
            })
            .map_err(|error| ErrorObject {
                code: codes::INVALID_PARAMS,
                message: format!("Bad task-wait parameters: {error}"),
                data: None,
            }),
        "start" => serde_json::from_value::<joyczl_protocol::TaskStartParams>(body.clone())
            .map(|params| {
                (
                    methods::TASK_START,
                    serde_json::to_value(params).expect("serialisable"),
                )
            })
            .map_err(|e| ErrorObject {
                code: codes::INVALID_PARAMS,
                message: format!("Bad task-start parameters: {e}"),
                data: None,
            }),
        "create" => serde_json::from_value::<TaskCreateParams>(body.clone())
            .map(|params| {
                (
                    methods::TASK_CREATE,
                    serde_json::to_value(params).expect("serialisable"),
                )
            })
            .map_err(|e| ErrorObject {
                code: codes::INVALID_PARAMS,
                message: format!("bad task-create parameters: {e}"),
                data: None,
            }),
        "suspend" | "resume" | "cancel" => serde_json::from_value::<TaskActionParams>(body.clone())
            .map(|params| {
                let method = match action.as_str() {
                    "suspend" => methods::TASK_SUSPEND,
                    "resume" => methods::TASK_RESUME,
                    _ => methods::TASK_CANCEL,
                };
                (method, serde_json::to_value(params).expect("serialisable"))
            })
            .map_err(|e| ErrorObject {
                code: codes::INVALID_PARAMS,
                message: format!("bad task-action parameters: {e}"),
                data: None,
            }),
        other => Err(ErrorObject {
            code: codes::INVALID_PARAMS,
            message: format!(
                "unknown task action: {other} (create/start/wait/signal/operations/reconcile/suspend/resume/cancel)"
            ),
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
/// Same discipline as `/api/task`: these are **human-only** settings — the model has no
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

/// `GET /api/schedules/jobs` — every declared job, with the scheduler's state merged in.
///
/// A failing `schedule/list` does **not** blank the panel: the declarations are read from
/// this machine's files and are exactly what a human can act on. The reason the state is
/// missing rides along as `error`, so "never ran" and "cannot tell" stay distinguishable.
pub async fn schedule_jobs(Extension(app): Extension<Arc<AppServer>>) -> Response {
    let (state, state_error) = match app
        .request::<ScheduleListResponse>(methods::SCHEDULE_LIST, json!({}))
        .await
    {
        Ok(response) => (response.jobs, None),
        Err(error) => (Vec::new(), Some(error.message)),
    };
    let home = app.home().to_path_buf();
    // Cron is wall-clock: `joy schedule` fires on local time, so the next-run column must
    // answer in the same clock or it would disagree with the runs it is predicting.
    let now = chrono::Local::now().naive_local();
    let (jobs, file_error) = crate::schedules::rows(&home, &state, now);
    Json(crate::schedules::SchedulesView {
        jobs,
        error: file_error.or(state_error),
    })
    .into_response()
}

/// `POST /api/schedules/save` — add or replace one job in `<home>/schedules.json`.
pub async fn schedule_save(
    Extension(app): Extension<Arc<AppServer>>,
    Json(body): Json<crate::schedules::JobBody>,
) -> Response {
    match crate::schedules::save(app.home(), &body) {
        Ok(message) => {
            eprintln!("(joy) capability hub: {message}");
            outcome(true, message)
        }
        Err(message) => refusal(message),
    }
}

/// `POST /api/schedules/remove` — delete one file-declared job (skill jobs are refused).
pub async fn schedule_remove(
    Extension(app): Extension<Arc<AppServer>>,
    Json(body): Json<capabilities::NameBody>,
) -> Response {
    match crate::schedules::remove(app.home(), body.name.trim()) {
        Ok(message) => {
            eprintln!("(joy) capability hub: {message}");
            outcome(true, message)
        }
        Err(message) => refusal(message),
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
                message: format!("the file is over the 20MB limit ({} bytes)", body.len()),
                data: None,
            },
        );
    }
    if body.is_empty() {
        return failed(
            StatusCode::BAD_REQUEST,
            ErrorObject {
                code: codes::INVALID_PARAMS,
                message: "Empty files are not accepted.".into(),
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
                message: format!("could not create the directory: {why}"),
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
                message: format!("could not write the file: {why}"),
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
        return Err(format!("'{raw}' must be an absolute path"));
    }
    if !path.is_dir() {
        return Err(format!("'{raw}' does not exist or is not a directory"));
    }
    let mut entries: Vec<FsEntry> = std::fs::read_dir(&path)
        .map_err(|why| format!("could not read {raw}: {why}"))?
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
        return Err(format!("parent '{parent_raw}' must be an absolute path"));
    }
    if !parent.is_dir() {
        return Err(format!(
            "parent '{parent_raw}' does not exist or is not a directory"
        ));
    }
    let name = name.trim();
    let bad = |why: &str| format!("directory name '{name}' is not valid: {why}");
    if name.is_empty() || name.len() > 64 {
        return Err(bad("1-64 characters"));
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") || name.starts_with('.') {
        return Err(bad(
            "no path shapes allowed (separators, .., a leading dot)",
        ));
    }
    let target = parent.join(name);
    if target.exists() {
        return Err(format!("'{name}' already exists under {parent_raw}"));
    }
    std::fs::create_dir(&target).map_err(|why| format!("could not create it: {why}"))?;
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
    let payload = serde_json::to_value(&params).expect("ModelSaveParams always serialises");
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
    let payload = serde_json::to_value(&params).expect("ModelRemoveParams always serialises");
    match app
        .request::<ModelRemoveResponse>(methods::MODEL_REMOVE, payload)
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// `GET /api/model/routes` — the routing table (fixed / auto + tier → profile id).
pub async fn model_routes(Extension(app): Extension<Arc<AppServer>>) -> Response {
    match app
        .request::<ModelRoutesResponse>(methods::MODEL_ROUTES, json!({}))
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// `POST /api/model/routes/save` — replace the whole table. Human-only, like `/model`.
pub async fn model_routes_save(
    Extension(app): Extension<Arc<AppServer>>,
    Json(params): Json<ModelRoutesSaveParams>,
) -> Response {
    let payload = serde_json::to_value(&params).expect("ModelRoutesSaveParams always serialises");
    match app
        .request::<ModelRoutesResponse>(methods::MODEL_ROUTES_SAVE, payload)
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
    let payload = serde_json::to_value(&params).expect("ApprovalRespondParams always serialises");
    // Same path as turn: the app-server is a subprocess, answering is one of its methods.
    match app
        .request::<ApprovalRespondResponse>(methods::APPROVAL_RESPOND, payload)
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// `POST /api/interrupt` — stop a turn that is running.
///
/// The payload is the protocol's `TurnInterruptParams` (**there is no second shape**); the
/// front end takes the turnId from the `turnStarted` event — it is that turn's only handle.
/// Too late (the turn already ended) → `interrupted: false`, and the front end says so
/// instead of pretending something was stopped.
pub async fn interrupt(
    Extension(app): Extension<Arc<AppServer>>,
    Json(params): Json<TurnInterruptParams>,
) -> Response {
    let payload = serde_json::to_value(&params).expect("TurnInterruptParams always serialises");
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
                message: "the message is empty".to_string(),
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

    let payload = serde_json::to_value(&params).expect("TurnStartParams always serialises");
    tokio::spawn(async move {
        forward(&app, payload, session_id, tx, events).await;
    });

    Sse::new(bytes_stream(rx))
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// `POST /api/run` — **the task API**: one turn in, one answer out, plain JSON.
///
/// Same body as `/api/turn` (`TurnStartParams`, no second shape), but no SSE: a program (CI,
/// cron, another agent) wants the reply, not a typewriter. The dashboard keeps SSE because a
/// person wants to watch the words arrive; this endpoint exists for callers that don't.
pub async fn run(
    Extension(app): Extension<Arc<AppServer>>,
    Json(params): Json<TurnStartParams>,
) -> Response {
    if params.message.trim().is_empty() {
        return failed(
            StatusCode::BAD_REQUEST,
            ErrorObject {
                code: codes::INVALID_PARAMS,
                message: "the message is empty".to_string(),
                data: None,
            },
        );
    }
    match run_headless(&app, params).await {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// One turn in, one answer out — shared by `/api/run` and inbound webhooks.
///
/// `turn/start` answers only when the turn is over, but its answer carries just the turn id;
/// the reply rides on `turnCompleted`. So subscribe **before** sending (same reason as `/api/turn`),
/// then wait. If the completion was lost to a full buffer while the request was still pending,
/// fall back to the newest assistant row — by then the turn is over and that row exists.
pub(crate) async fn run_headless(
    app: &Arc<AppServer>,
    params: TurnStartParams,
) -> Result<RunResponse, ErrorObject> {
    let session_id = params
        .session_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "default".to_string());
    let payload = serde_json::to_value(&params).expect("TurnStartParams always serialises");
    let mut events = app.subscribe();
    let request = app.request::<TurnStartResponse>(methods::TURN_START, payload);
    tokio::pin!(request);

    let mut turn_id: Option<String> = None;
    loop {
        tokio::select! {
            result = &mut request => {
                result?;
                // Normally unreachable (completion is sent before the response); kept so a lost
                // notification cannot turn into a hung HTTP request.
                break;
            }
            received = events.recv() => {
                let notification = match received {
                    Ok(notification) => notification,
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        eprintln!("(joy) the task API's notifications arrived too late; {missed} were dropped");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let Some(id) = belongs(&notification, &session_id, turn_id.as_deref()) else {
                    continue;
                };
                turn_id = Some(id);
                if let Some(run) = completed_turn(&notification) {
                    return Ok(run);
                }
            }
        }
    }

    // The response and the completion travel the same pipe, completion first: by the time the
    // request resolves, the notification is already queued. Drain it rather than paying a read.
    while let Ok(notification) = events.try_recv() {
        if let Some(run) = completed_turn(&notification) {
            return Ok(run);
        }
    }

    // Fallback: the turn is over, the row is written; read the last assistant message.
    let last = app
        .request::<SessionMessagesResponse>(
            methods::SESSION_MESSAGES,
            json!({ "sessionId": session_id, "limit": 1 }),
        )
        .await?;
    let reply = last
        .data
        .into_iter()
        .find(|message| matches!(message.role, MessageRole::Assistant))
        .ok_or_else(|| ErrorObject {
            code: codes::INTERNAL_ERROR,
            message: "the turn finished, but the reply could not be read (the notification queue overflowed and it is not in the history)".to_string(),
            data: None,
        })?;
    let meta = reply.meta;
    Ok(RunResponse {
        turn_id: turn_id.unwrap_or_default(),
        reply: reply.content,
        model: meta.as_ref().map(|m| m.model.clone()).unwrap_or_default(),
        provider: meta
            .as_ref()
            .map(|m| m.provider.clone())
            .unwrap_or_default(),
        route_reason: meta.and_then(|m| m.route_reason),
    })
}

/// `POST /api/hook/:name` — an inbound webhook runs one turn (see `webhooks.rs` for the fences).
///
/// Deliberately **outside** the dashboard token layer: a foreign machine proves itself with the
/// hook's own secret (`X-Joy-Secret`), not with a token it was never given. Single-user mode only —
/// in hub mode there are many homes and an inbound hook has no user to pick, so the door is 404.
pub async fn hook(
    State(gate): State<crate::Gate>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let Some(app) = gate.single_app() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let config = match webhooks::load(app.home()) {
        Ok(config) => config,
        Err(error) => {
            // A broken file disables every hook loudly — same discipline as connectors.json.
            eprintln!(
                "(joy) webhooks.json has a problem; inbound hooks are all disabled: {error:#}"
            );
            return failed(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorObject {
                    code: codes::INTERNAL_ERROR,
                    message: format!("webhooks.json has a problem: {error}"),
                    data: None,
                },
            );
        }
    };
    let Some(hook) = webhooks::find(&config, &name).filter(|hook| hook.enabled()) else {
        // Unknown and switched-off are the same answer: this door does not exist.
        return StatusCode::NOT_FOUND.into_response();
    };
    let credential = webhooks::Credential {
        secret: header(&headers, "x-joy-secret"),
        timestamp: header(&headers, "x-joy-timestamp"),
        signature: header(&headers, "x-joy-signature"),
    };
    if let Err(refusal) = webhooks::admit(
        hook,
        &credential,
        &body,
        chrono::Utc::now().timestamp(),
        std::time::Instant::now(),
    ) {
        return refusal_response(refusal);
    }

    let session = hook.session_id();
    let hook_name = hook.name.clone();
    let params = TurnStartParams {
        session_id: Some(session.clone()),
        message: hook.render(&body),
        // The caller wants one answer, not a stream; text deltas would only fill the buffer.
        stream: Some(false),
        cwd: None,
        attachments: None,
        tier: hook.tier.clone(),
    };

    // Async: acknowledge now, push the answer to the callback. The caller is a program that may
    // time out in 5s, while a turn can take a minute — holding the socket would only buy retries.
    if let Some(callback) = hook.callback_url().map(str::to_string) {
        let pushed_session = session.clone();
        tokio::spawn(async move {
            let payload = match run_headless(&app, params).await {
                Ok(run) => json!({
                    "hook": hook_name,
                    "session": pushed_session,
                    "ok": true,
                    "result": run,
                }),
                Err(error) => json!({
                    "hook": hook_name,
                    "session": pushed_session,
                    "ok": false,
                    "error": error,
                }),
            };
            webhooks::deliver(&callback, payload).await;
        });
        return (
            StatusCode::ACCEPTED,
            Json(webhooks::accepted(&hook.name, &session)),
        )
            .into_response();
    }

    match run_headless(&app, params).await {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// One header as text; absent or non-UTF8 reads as empty (the policy treats empty as absent).
fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

/// A refusal a script can read: the status is the contract, this sentence is for the operator.
fn refuse(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn refusal_response(refusal: webhooks::Refusal) -> Response {
    match refusal {
        webhooks::Refusal::Unauthorized(reason) => refuse(StatusCode::UNAUTHORIZED, reason),
        webhooks::Refusal::Replay => refuse(
            StatusCode::CONFLICT,
            "this signature has already been used (replay refused)",
        ),
        webhooks::Refusal::TooFast(wait) => {
            let mut response = refuse(
                StatusCode::TOO_MANY_REQUESTS,
                &format!("this hook's per-minute quota is spent; try again in {wait} seconds"),
            );
            if let Ok(value) = axum::http::HeaderValue::from_str(&wait.to_string()) {
                response.headers_mut().insert("retry-after", value);
            }
            response
        }
    }
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
                        eprintln!("(joy) notifications came too fast; {missed} were dropped this turn");
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

/// `turnCompleted` is the one-shot answer: reply + which model actually ran. Anything else: None.
fn completed_turn(notification: &ServerNotification) -> Option<RunResponse> {
    let ServerNotification::TurnCompleted(done) = notification else {
        return None;
    };
    Some(RunResponse {
        turn_id: done.turn_id.clone(),
        reply: done.reply.clone(),
        model: done.meta.model.clone(),
        provider: done.meta.provider.clone(),
        route_reason: done.meta.route_reason.clone(),
    })
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
        N::ToolDelta(n) => Some(&n.turn_id),
        N::ToolCompleted(n) => Some(&n.turn_id),
        N::TurnCompleted(n) => Some(&n.turn_id),
        N::TurnCheckpointed(n) => Some(&n.turn_id),
        N::TurnResumed(n) => Some(&n.turn_id),
        N::TurnSteered(n) => Some(&n.turn_id),
        N::TurnSuspended(n) => Some(&n.turn_id),
        N::TaskUpdated(_)
        | N::InboxReceived(_)
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
        let json = serde_json::to_string(&notification).expect("a notification always serialises");
        Some((Ok(Event::default().data(json)), rx))
    })
}

/// On error, return the protocol's own `ErrorObject`, untouched.
///
/// The status code is not 200: on the `fetch` side "this trip failed" is obvious at a
/// glance, while the body is still the familiar `{code, message}` — the page can just show
/// the message; what app-server wrote was meant for users in the first place.
pub(crate) fn failed(status: StatusCode, error: ErrorObject) -> Response {
    (status, Json(error)).into_response()
}

// ---- Capabilities: skills / MCP connectors ---------------------------------------
//
// The domain (rows, fences, validation) lives in `capabilities.rs`; this is the HTTP
// face, same as everything above. Built-in rows are refused by the write paths, so the
// handlers only translate: the server's sentence goes to the page verbatim.

/// A refusal the page can show verbatim: 200 + the protocol's `ErrorObject`, the same
/// convention as `/api/task` — a human action refused is an answer, not a fault.
fn refusal(message: String) -> Response {
    failed(
        StatusCode::OK,
        ErrorObject {
            code: codes::INVALID_PARAMS,
            message,
            data: None,
        },
    )
}

fn outcome(ok: bool, message: String) -> Response {
    Json(capabilities::Outcome { ok, message }).into_response()
}

/// `GET /api/skills` — what Joy can do procedurally, and which rows are built-in.
pub async fn skills(Extension(app): Extension<Arc<AppServer>>) -> Response {
    let (skills, error) = capabilities::skill_rows(app.home());
    Json(capabilities::SkillsView { skills, error }).into_response()
}

/// `POST /api/skills/install` — paste a SKILL.md or give a URL.
///
/// Validation lives in `joyczl-memory` (the same gate `joy skill install` passes), so
/// the page cannot install something the CLI would refuse. Skills take effect on the
/// next turn (the loader rescans on directory changes — no restart needed).
pub async fn skills_install(
    Extension(app): Extension<Arc<AppServer>>,
    Json(body): Json<capabilities::SkillInstallBody>,
) -> Response {
    let home = app.home().to_path_buf();
    let result = if !body.url.trim().is_empty() {
        joyczl_memory::skills::install_from_url(&home, body.url.trim()).await
    } else if !body.text.trim().is_empty() {
        joyczl_memory::skills::install_from_text(&home, &body.text)
    } else {
        Err(
            "Paste SKILL.md content or give a URL — with both empty there is nothing to install"
                .to_string(),
        )
    };
    match result {
        Ok(message) => {
            eprintln!("(joy) capability hub: {message}");
            outcome(true, message)
        }
        Err(message) => refusal(message),
    }
}

/// `POST /api/skills/remove` — delete a user skill (built-in ones are refused).
pub async fn skills_remove(
    Extension(app): Extension<Arc<AppServer>>,
    Json(body): Json<capabilities::NameBody>,
) -> Response {
    match capabilities::remove_skill(app.home(), body.name.trim()) {
        Ok(message) => {
            eprintln!("(joy) capability hub: {message}");
            outcome(true, message)
        }
        Err(message) => refusal(message),
    }
}

/// `GET /api/mcp` — the configured connectors and how each one authenticates.
pub async fn mcp(Extension(app): Extension<Arc<AppServer>>) -> Response {
    let (servers, error) = capabilities::mcp_rows(app.home());
    Json(capabilities::McpView { servers, error }).into_response()
}

/// `POST /api/mcp/save` — add or replace one connector in `<home>/mcp.json`.
///
/// The body is `joyczl-mcp`'s `ServerSpec` (**no second shape**); a bad one is
/// translated into a sentence here rather than axum's default 400 wall.
pub async fn mcp_save(
    Extension(app): Extension<Arc<AppServer>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let spec: ServerSpec = match serde_json::from_value(body) {
        Ok(spec) => spec,
        Err(why) => return refusal(format!("bad connector parameters: {why}")),
    };
    match capabilities::save_server(app.home(), &spec) {
        Ok(message) => {
            eprintln!("(joy) capability hub: {message}");
            outcome(
                true,
                format!("{message}\nEffective after a restart — connections happen at startup"),
            )
        }
        Err(message) => refusal(message),
    }
}

/// `POST /api/mcp/remove` — delete a user connector (built-in ones are refused).
pub async fn mcp_remove(
    Extension(app): Extension<Arc<AppServer>>,
    Json(body): Json<capabilities::NameBody>,
) -> Response {
    match capabilities::remove_server(app.home(), body.name.trim()) {
        Ok(message) => {
            eprintln!("(joy) capability hub: {message}");
            outcome(
                true,
                format!("{message}\nEffective after a restart — connections happen at startup"),
            )
        }
        Err(message) => refusal(message),
    }
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod api_tests;

/// Human inspection/reconciliation of stopped ordinary-chat tool operations.
pub async fn turn_operation(
    Extension(app): Extension<Arc<AppServer>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let action = body
        .get("action")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let translated = match action {
        "operations" => serde_json::from_value::<joyczl_protocol::TurnOperationsParams>(
            body.clone(),
        )
        .map(|params| {
            (
                methods::TURN_OPERATIONS,
                serde_json::to_value(params).expect("serialisable"),
            )
        }),
        "reconcile" => serde_json::from_value::<joyczl_protocol::TurnReconcileParams>(body.clone())
            .map(|params| {
                (
                    methods::TURN_RECONCILE,
                    serde_json::to_value(params).expect("serialisable"),
                )
            }),
        _ => {
            return failed(
                StatusCode::OK,
                ErrorObject {
                    code: codes::INVALID_PARAMS,
                    message: "Unknown chat operation action".into(),
                    data: None,
                },
            )
        }
    };
    let result = match translated {
        Ok((method, params)) => {
            app.request::<joyczl_protocol::TaskOperationsResponse>(method, params)
                .await
        }
        Err(error) => Err(ErrorObject {
            code: codes::INVALID_PARAMS,
            message: format!("Invalid chat operation parameters: {error}"),
            data: None,
        }),
    };
    match result {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}
