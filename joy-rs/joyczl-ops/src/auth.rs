//! The web account endpoints: `/api/auth/register` and `/api/auth/login`.
//!
//! Deliberately thin. All rules (usernames, seats, the first-user gate, PBKDF2, the
//! anti-enumeration dummy pass) live in `hub.rs` — this file only maps verdicts to
//! HTTP: 422 bad input, 409 taken, 403 closed/full, 401 wrong password, 200 a token.
//!
//! These routes are registered **after** the authorize layer in the router, so they are
//! reachable without a token — that is the whole point. In single-user mode they answer
//! 400: there is nothing to log into, the page is already talking to its own server.

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::hub::AuthError;
use crate::Gate;

/// One account form: who, what secret, which device. camelCase on the wire.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthParams {
    pub username: String,
    pub password: String,
    /// A label for this device ("我的手机", "work-laptop"). Same device + password
    /// returns the same token — logging in twice does not multiply tokens.
    pub device: String,
}

pub async fn register(State(gate): State<Gate>, Json(params): Json<AuthParams>) -> Response {
    let Some(hub) = gate.hub_ref() else {
        return error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "本机模式不需要登录——这个驾驶舱就是你自己的服务器",
        );
    };
    match hub.register(&params.username, &params.password, &params.device) {
        Ok(token) => Json(json!({ "user": params.username, "token": token })).into_response(),
        Err(error) => error_response(status_of(&error), message_of(&error)),
    }
}

pub async fn login(State(gate): State<Gate>, Json(params): Json<AuthParams>) -> Response {
    let Some(hub) = gate.hub_ref() else {
        return error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "本机模式不需要登录——这个驾驶舱就是你自己的服务器",
        );
    };
    match hub.login(&params.username, &params.password, &params.device) {
        Ok(token) => Json(json!({ "user": params.username, "token": token })).into_response(),
        Err(error) => error_response(status_of(&error), message_of(&error)),
    }
}

fn status_of(error: &AuthError) -> axum::http::StatusCode {
    match error {
        AuthError::BadInput(_) => axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        AuthError::Invalid(_) => axum::http::StatusCode::UNAUTHORIZED,
        AuthError::Exists => axum::http::StatusCode::CONFLICT,
        AuthError::Full | AuthError::Closed => axum::http::StatusCode::FORBIDDEN,
        AuthError::Internal => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Register's input messages are rules worth showing; login's are one honest line
/// (never say which half was wrong).
fn message_of(error: &AuthError) -> &'static str {
    match error {
        AuthError::BadInput(message) | AuthError::Invalid(message) => message,
        AuthError::Exists => "这个用户名已经被注册了",
        AuthError::Full => "用户名额已满（max_open）——注册关闭",
        AuthError::Closed => {
            "这台服务器还不允许公网注册第一个用户（先在本机注册，或 joy hub adduser）"
        }
        AuthError::Internal => "服务器内部错误（token 生成或存盘失败）",
    }
}

/// The dashboard's `errorOf` understands `{code, message}` — keep that shape here too.
fn error_response(status: axum::http::StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({ "code": status.as_u16(), "message": message })),
    )
        .into_response()
}
