//! Schedules HTTP endpoints.

use axum::extract::Extension;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::Json;
use std::sync::Arc;

use super::{failed, outcome, refusal};
use crate::{capabilities, AppServer};
use joyczl_protocol::{methods, ScheduleListResponse};
use serde_json::json;

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
