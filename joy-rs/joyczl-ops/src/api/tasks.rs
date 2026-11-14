//! Tasks HTTP endpoints.

use super::failed;
use crate::AppServer;
use axum::extract::Extension;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::Json;
use joyczl_protocol::{
    codes, methods, ErrorObject, TaskActionParams, TaskCreateParams, TaskEnvelope, TaskListResponse,
};
use serde_json::json;
use std::sync::Arc;

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
