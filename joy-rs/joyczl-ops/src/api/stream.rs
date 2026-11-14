//! Stream HTTP endpoints.

use super::failed;
use crate::AppServer;
use axum::extract::Extension;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::response::Response;
use axum::Json;
use futures_util::stream::{self, Stream};
use joyczl_protocol::{
    codes, methods, ErrorObject, MessageRole, RunResponse, ServerNotification,
    SessionMessagesResponse, TurnStartParams, TurnStartResponse,
};
use serde_json::json;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

/// How many not-yet-consumed notifications may pile up. **Unbounded** on purpose: bounded
/// would stall the forwarding task on send, leaving the broadcast channel unconsumed —
/// intermediate `textDelta`s get dropped, possibly along with `turnCompleted`, and that
/// SSE turn would never end. The unbounded cap is one full turn's worth of notifications,
/// and the channel is torn down when the turn ends.
pub(super) const TURN_BUFFER: usize = 1024;

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

/// Forward this turn's notifications to the browser one by one (subscriptions + response).
///
/// The request's response (the `turn/start` return) is usually **ignored by everyone** —
/// `turnCompleted` already carries back reply, usage and telemetry. But the request may
/// fail outright (bad model config or params); then it is the only word, and must be shown.
pub(super) async fn forward(
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
pub(super) fn completed_turn(notification: &ServerNotification) -> Option<RunResponse> {
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
pub(super) fn belongs(
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
pub(super) fn turn_id_of(notification: &ServerNotification) -> Option<&str> {
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
pub(super) fn bytes_stream(
    rx: mpsc::Receiver<ServerNotification>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    stream::unfold(rx, |mut rx| async move {
        let notification = rx.recv().await?;
        let json = serde_json::to_string(&notification).expect("a notification always serialises");
        Some((Ok(Event::default().data(json)), rx))
    })
}
