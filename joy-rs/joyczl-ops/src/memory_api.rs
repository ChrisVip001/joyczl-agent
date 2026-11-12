//! The memory center's HTTP endpoints — thin forwarders to the protocol.
//!
//! Same discipline as `api.rs`: every one of these is a **read** except `compact`, which is
//! the human's own button. No business logic lives here; the app-server owns what the ledger
//! and the bundle mean. The sandbox note: page slugs are passed straight through, and the
//! bundle's own `page_path` refuses anything that escapes `<home>/memory` — the check is
//! next to the filesystem call, not duplicated at the HTTP edge.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Extension, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use joyczl_protocol::{
    codes, methods, ContextUsageResponse, ErrorObject, MemoryIndexResponse, MemoryLedgerParams,
    MemoryLedgerResponse, MemoryPageParams, MemoryPageResponse, SessionCompactParams,
    SessionCompactResponse, SkillStatsResponse,
};
use serde_json::json;

use crate::api::failed;
use crate::AppServer;

/// `GET /api/ledger/turns?sessionId=&limit=` — one session's ledger, newest first.
pub async fn ledger_turns(
    Extension(app): Extension<Arc<AppServer>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(session_id) = required(&params, "sessionId") else {
        return missing("sessionId");
    };
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(30);
    let payload = json!(MemoryLedgerParams {
        session_id,
        limit: Some(limit),
        turn_id: None,
    });
    forward::<MemoryLedgerResponse>(&app, methods::MEMORY_LEDGER, payload).await
}

/// `GET /api/ledger/turn?sessionId=&turnId=` — the turn list plus that turn's entries.
///
/// The list rides along so the panel can highlight the row it opened without a second
/// request; the entries are the only part that differs.
pub async fn ledger_turn(
    Extension(app): Extension<Arc<AppServer>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(session_id) = required(&params, "sessionId") else {
        return missing("sessionId");
    };
    let Some(turn_id) = required(&params, "turnId") else {
        return missing("turnId");
    };
    let payload = json!(MemoryLedgerParams {
        session_id,
        limit: Some(30),
        turn_id: Some(turn_id),
    });
    forward::<MemoryLedgerResponse>(&app, methods::MEMORY_LEDGER, payload).await
}

/// `GET /api/memory/index` — the bundle's catalog.
pub async fn memory_index(Extension(app): Extension<Arc<AppServer>>) -> Response {
    forward::<MemoryIndexResponse>(&app, methods::MEMORY_INDEX, json!({})).await
}

/// `GET /api/memory/page?page=<slug>` — one page with provenance.
pub async fn memory_page(
    Extension(app): Extension<Arc<AppServer>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(page) = required(&params, "page") else {
        return missing("page");
    };
    forward::<MemoryPageResponse>(&app, methods::MEMORY_PAGE, json!(MemoryPageParams { page }))
        .await
}

/// `GET /api/context?sessionId=` — how the last turn spent the window.
pub async fn context(
    Extension(app): Extension<Arc<AppServer>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(session_id) = required(&params, "sessionId") else {
        return missing("sessionId");
    };
    forward::<ContextUsageResponse>(
        &app,
        methods::CONTEXT_USAGE,
        json!(joyczl_protocol::ContextUsageParams { session_id }),
    )
    .await
}

/// `GET /api/skills/stats` — how often each skill actually fired.
pub async fn skill_stats(Extension(app): Extension<Arc<AppServer>>) -> Response {
    forward::<SkillStatsResponse>(&app, methods::SKILL_STATS, json!({})).await
}

/// `POST /api/compact?sessionId=&focus=` — compact now, on the human's word.
///
/// POST because it spends a model call and writes the rollup: a GET that changes state is
/// the kind of thing a browser prefetches.
pub async fn compact(
    Extension(app): Extension<Arc<AppServer>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(session_id) = required(&params, "sessionId") else {
        return missing("sessionId");
    };
    let focus = params
        .get("focus")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    forward::<SessionCompactResponse>(
        &app,
        methods::SESSION_COMPACT,
        json!(SessionCompactParams { session_id, focus }),
    )
    .await
}

// ---- Small helpers ---------------------------------------------------------------------

async fn forward<T: serde::de::DeserializeOwned + serde::Serialize>(
    app: &Arc<AppServer>,
    method: &str,
    payload: serde_json::Value,
) -> Response {
    match app.request::<T>(method, payload).await {
        Ok(response) => Json(response).into_response(),
        // 200 with an `ErrorObject`, exactly like the rest of `/api/*`: the dashboard's
        // `outcomeOf` reads `ok`, and a non-2xx would make it guess.
        Err(error) => failed(StatusCode::OK, error),
    }
}

fn required(params: &HashMap<String, String>, key: &str) -> Option<String> {
    params
        .get(key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn missing(key: &str) -> Response {
    failed(
        StatusCode::OK,
        ErrorObject {
            code: codes::INVALID_PARAMS,
            message: format!("缺少参数 {key}"),
            data: None,
        },
    )
}
