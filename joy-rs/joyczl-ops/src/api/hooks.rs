//! Hooks HTTP endpoints.

use super::{failed, run_headless};
use crate::webhooks;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::response::Response;
use axum::Json;
use joyczl_protocol::{codes, ErrorObject, TurnStartParams};
use serde_json::json;

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
pub(super) fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

/// A refusal a script can read: the status is the contract, this sentence is for the operator.
pub(super) fn refuse(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

pub(super) fn refusal_response(refusal: webhooks::Refusal) -> Response {
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
