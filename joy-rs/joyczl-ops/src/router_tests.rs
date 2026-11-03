//! Auth tests for the HTTP face: token three states, binding validation, constant-time compare.
//!
//! Hits `router()` directly (serve with a real listener is integration smoke territory);
//! the app-server uses the same fake as client_tests — an auto-responder answers `{}`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use crate::client::client_tests::fake;
use crate::{constant_time_eq, router, validate_binding, Gate};

/// Start an auto-responder: minimal valid answers per method. **Must answer** — with the
/// fake end gone, `app.request` would wait forever (closed link is not an answer).
fn auto_respond(fake: crate::client::client_tests::Fake) {
    tokio::spawn(async move {
        let mut fake = fake;
        loop {
            let request = fake.next_request().await;
            let method = request["method"].as_str().unwrap_or_default().to_string();
            let id = request["id"].clone();
            let result = match method.as_str() {
                "config/read" => json!({"config": {
                    "provider": "anthropic", "model": "", "smallModel": "",
                    "maxIterations": 10, "maxTokens": 4096, "historyTurns": 10,
                    "consolidateEvery": 5, "retrievalTopK": 5,
                    "appleCalendar": false, "googleCalendar": false,
                    "experimental": false, "graphWorkflows": false, "home": "/tmp",
                }}),
                _ => json!({"data": []}),
            };
            fake.respond(&id, result).await;
        }
    });
}

/// Fire one request, return the status code.
async fn status_of(router: axum::Router, path: &str, auth: Option<&str>) -> StatusCode {
    let mut builder = Request::builder().uri(path);
    if let Some(token) = auth {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = router
        .oneshot(builder.body(Body::empty()).expect("请求体"))
        .await
        .expect("路由响应");
    response.status()
}

/// Token configured: missing / wrong → 401; correct → pass.
#[tokio::test]
async fn a_token_gates_the_api() {
    let (app, fake) = fake();
    auto_respond(fake);
    let router = router(
        Gate::single(Arc::new(app), Some("s3cret".to_string())),
        std::env::temp_dir(),
    );

    assert_eq!(
        status_of(router.clone(), "/api/data", None).await,
        StatusCode::UNAUTHORIZED,
        "不带 token 不该放行"
    );
    assert_eq!(
        status_of(router.clone(), "/api/data", Some("错的")).await,
        StatusCode::UNAUTHORIZED,
        "错 token 不该放行"
    );
    assert_eq!(
        status_of(router.clone(), "/api/data", Some("s3cret")).await,
        StatusCode::OK,
        "对 token 该放行"
    );
    // Even the Bearer prefix is required — a bare token in the header is not enough.
    let response = router
        .oneshot(
            Request::builder()
                .uri("/api/data")
                .header("authorization", "s3cret")
                .body(Body::empty())
                .expect("请求体"),
        )
        .await
        .expect("路由响应");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// No token configured: everything passes locally by default (unchanged behavior).
#[tokio::test]
async fn without_a_token_the_api_is_open() {
    let (app, fake) = fake();
    auto_respond(fake);
    let router = router(Gate::single(Arc::new(app), None), std::env::temp_dir());
    assert_eq!(
        status_of(router, "/api/data", None).await,
        StatusCode::OK,
        "没配 token 时中间件不该拦任何请求"
    );
}

/// Token covers /api/* only: static files carry no data and must not be blocked by 401
/// (the page itself must load; the token is sent by the page JS in request headers).
#[tokio::test]
async fn static_files_are_not_gated() {
    let (app, _fake) = fake();
    let dir = tempfile::tempdir().expect("临时目录");
    let router = router(
        Gate::single(Arc::new(app), Some("s3cret".to_string())),
        dir.path().to_path_buf(),
    );
    let response = router
        .oneshot(
            Request::builder()
                .uri("/")
                .body(Body::empty())
                .expect("请求体"),
        )
        .await
        .expect("路由响应");
    assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
}

/// Binding validation: non-loopback + no token = reject; other combinations all have a way.
#[test]
fn a_non_loopback_host_needs_a_token() {
    assert!(validate_binding("127.0.0.1", None).is_ok());
    assert!(validate_binding("localhost", None).is_ok());
    assert!(validate_binding("::1", None).is_ok());
    assert!(validate_binding("0.0.0.0", Some("t")).is_ok());
    assert!(validate_binding("192.168.1.5", Some("t")).is_ok());

    let error = validate_binding("0.0.0.0", None).unwrap_err();
    assert!(error.to_string().contains("JOY_OPS_TOKEN"), "{error}");
    let error = validate_binding("192.168.1.5", None).unwrap_err();
    assert!(error.to_string().contains("JOY_OPS_TOKEN"), "{error}");
}

/// Constant-time compare: equal, unequal, different lengths — same semantics as ==.
#[test]
fn constant_time_comparison_agrees_with_equality() {
    assert!(constant_time_eq(b"abc", b"abc"));
    assert!(!constant_time_eq(b"abc", b"abd"));
    assert!(!constant_time_eq(b"abc", b"abcd"));
    assert!(constant_time_eq(b"", b""));
}
