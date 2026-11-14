//! HTTP route facade; resource modules translate the app-server protocol.

use crate::capabilities;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::Json;
use joyczl_protocol::{codes, ErrorObject};

mod overview;
pub use overview::approvals;
pub use overview::data;
pub use overview::inbox;
pub use overview::usage;
mod sessions;
pub use sessions::approval;
pub use sessions::follow_up;
pub use sessions::interrupt;
pub use sessions::session;
pub use sessions::session_fork;
pub use sessions::session_meta;
pub use sessions::session_new;
pub use sessions::session_update;
pub use sessions::steer;
pub use sessions::turns;
mod tasks;
pub use tasks::task;
pub use tasks::tasks;
pub use tasks::turn_operation;
mod schedules;
pub use schedules::schedule_jobs;
pub use schedules::schedule_remove;
pub use schedules::schedule_save;
pub use schedules::schedules;
mod files;

pub use files::fs_browse;
pub use files::fs_mkdir;
pub use files::fs_recents;
pub use files::fs_recents_record;

pub use files::upload;
mod models;
pub use models::mcp;
pub use models::mcp_remove;
pub use models::mcp_save;
pub use models::model_profiles;
pub use models::model_remove;
pub use models::model_routes;
pub use models::model_routes_save;
pub use models::model_save;
pub use models::skills;
pub use models::skills_install;
pub use models::skills_remove;
mod stream;

pub use stream::events;
pub use stream::run;
pub(crate) use stream::run_headless;
pub use stream::turn;
mod hooks;
pub use hooks::hook;

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

#[cfg(test)]
use files::{create_subdir, list_dirs, safe_extension};
#[cfg(test)]
use stream::belongs;
#[cfg(test)]
pub(crate) use stream::EventPing;
#[cfg(test)]
#[path = "api_tests.rs"]
mod api_tests;
