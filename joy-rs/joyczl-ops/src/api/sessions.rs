//! Sessions HTTP endpoints.

use super::failed;
use crate::AppServer;
use axum::extract::{Extension, Query};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::Json;
use joyczl_protocol::{
    methods, ApprovalRespondParams, ApprovalRespondResponse, SessionForkParams,
    SessionForkResponse, SessionGetParams, SessionGetResponse, SessionMessagesParams,
    SessionMessagesResponse, SessionNewParams, SessionNewResponse, SessionUpdateParams,
    SessionUpdateResponse, TurnFollowUpParams, TurnFollowUpResponse, TurnInterruptParams,
    TurnInterruptResponse, TurnListParams, TurnListResponse, TurnSteerParams, TurnSteerResponse,
};
use std::sync::Arc;

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
