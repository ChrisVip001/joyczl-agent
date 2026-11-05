//! Events + CORS tests: the ping mapping, the events channel's auth gate and content
//! type, and the preflight a Tauri/PWA client needs.
//!
//! A full "two clients see the same ping" run needs a live turn and a held-open SSE
//! body — that is smoke territory (smoke.sh drives the real server); here we pin the
//! pure parts and the headers.

use super::api::EventPing;
use super::router_tests::{auto_respond, fake, status_of};
use axum::body::Body;
use axum::http::{header, HeaderValue, Request, StatusCode};
use axum::Router;
use joyczl_protocol::{ServerNotification, TurnStartedNotification};
use std::sync::Arc;
use tower::ServiceExt;

fn router_with_token() -> Router {
    let (app, fake) = fake();
    auto_respond(fake);
    super::router(
        super::Gate::single(Arc::new(app), Some("s3cret".to_string())),
        std::env::temp_dir(),
    )
}

/// Only meaningful changes become pings; the per-token flow noise is filtered out.
#[test]
fn pings_distill_the_notifications_that_matter() {
    let started = ServerNotification::TurnStarted(TurnStartedNotification {
        turn_id: "t-1".to_string(),
        session_id: "default".to_string(),
        user_message: "你好".to_string(),
        ts: "2026-11-05T00:00:00.000Z".to_string(),
    });
    let ping = EventPing::of(&started).expect("回合开始是一条 ping");
    assert_eq!(ping.kind, "turn_started");
    assert_eq!(ping.session_id.as_deref(), Some("default"));

    let noise = ServerNotification::TextDelta(joyczl_protocol::TextDeltaNotification {
        turn_id: "t-1".to_string(),
        delta: "一个".to_string(),
    });
    assert!(EventPing::of(&noise).is_none(), "逐字流不打门");
}

/// The events channel is inside the authorize layer: no token → 401; with one → an SSE
/// response with the right content type (the body is an infinite stream — headers only).
#[tokio::test]
async fn the_events_channel_is_guarded_and_sse() {
    let router = router_with_token();

    assert_eq!(
        status_of(router.clone(), "/api/events", None).await,
        StatusCode::UNAUTHORIZED,
        "事件流和别的 API 一样要 token"
    );

    let response = router
        .oneshot(
            Request::builder()
                .uri("/api/events")
                .header("authorization", "Bearer s3cret")
                .body(Body::empty())
                .expect("请求体"),
        )
        .await
        .expect("路由响应");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .is_some_and(|value| value
                .to_str()
                .unwrap_or("")
                .starts_with("text/event-stream")),
        "得是 SSE 的 content-type"
    );
}

/// A Tauri/PWA client's preflight passes and gets the allow-origin header back.
#[tokio::test]
async fn cors_preflight_passes_for_api() {
    let router = router_with_token();
    let response = router
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/api/data")
                .header(header::ORIGIN, "tauri://localhost")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                .body(Body::empty())
                .expect("请求体"),
        )
        .await
        .expect("路由响应");
    assert!(
        response.status().is_success(),
        "预检该过：{:?}",
        response.status()
    );
    assert_eq!(
        response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
        Some(&HeaderValue::from_static("*")),
    );
}

/// The long-task engine's knocks distill like any other: a boundary, a resumed turn and a
/// closed checkpoint each become one ping carrying the turn and session ids.
#[test]
fn task_pings_distill_the_checkpoint_lifecycle() {
    let checkpointed =
        ServerNotification::TurnCheckpointed(joyczl_protocol::TurnCheckpointedNotification {
            turn_id: "t-crash".to_string(),
            session_id: "default".to_string(),
            iterations: 3,
        });
    let ping = EventPing::of(&checkpointed).expect("边界落盘是一条 ping");
    assert_eq!(ping.kind, "task_progress");
    assert_eq!(ping.turn_id.as_deref(), Some("t-crash"));

    let resumed = ServerNotification::TurnResumed(joyczl_protocol::TurnResumedNotification {
        turn_id: "t-crash".to_string(),
        session_id: "default".to_string(),
        from_iterations: 3,
        reason: "reconcile: crash recovery".to_string(),
    });
    let ping = EventPing::of(&resumed).expect("续跑是一条 ping");
    assert_eq!(ping.kind, "task_resumed");
    assert_eq!(ping.session_id.as_deref(), Some("default"));

    let suspended = ServerNotification::TurnSuspended(joyczl_protocol::TurnSuspendedNotification {
        turn_id: "t-crash".to_string(),
        session_id: "default".to_string(),
        reason: "interrupted by user".to_string(),
    });
    let ping = EventPing::of(&suspended).expect("挂起是一条 ping");
    assert_eq!(ping.kind, "task_suspended");
}
