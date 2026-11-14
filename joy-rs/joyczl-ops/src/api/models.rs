//! Models HTTP endpoints.

use super::{failed, outcome, refusal};
use crate::{capabilities, AppServer};
use axum::extract::Extension;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::Json;
use joyczl_mcp::config::ServerSpec;
use joyczl_protocol::{
    methods, ModelProfilesResponse, ModelRemoveParams, ModelRemoveResponse, ModelRoutesResponse,
    ModelRoutesSaveParams, ModelSaveParams, ModelSaveResponse,
};
use serde_json::json;
use std::sync::Arc;

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
