//! HTTP 面的鉴权测试：token 三态、绑定校验、常量时间比较。
//!
//! 直接打 `router()`（serve 起真监听是集成冒烟的事），app-server 用
//! client_tests 的同一套 fake —— 自动应答器把协议请求全回 `{}`。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use crate::client::client_tests::fake;
use crate::{constant_time_eq, router, validate_binding};

/// 起一个自动应答器：按方法给最小合法应答。**必须回** —— fake 一端断开
/// 后 `app.request` 还是会永远等（连接关了不等于应答到了），oneshot 就挂死。
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

/// 打一个请求，返回状态码。
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

/// 配了 token：不带 / 带错 → 401；带对 → 放行。
#[tokio::test]
async fn a_token_gates_the_api() {
    let (app, fake) = fake();
    auto_respond(fake);
    let router = router(
        Arc::new(app),
        std::env::temp_dir(),
        Some("s3cret".to_string()),
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
    // Bearer 前缀都不能少 —— 直接把裸 token 放进头里也不行。
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

/// 没配 token：本机默认全放行（现状不变，驾驶舱体验不受影响）。
#[tokio::test]
async fn without_a_token_the_api_is_open() {
    let (app, fake) = fake();
    auto_respond(fake);
    let router = router(Arc::new(app), std::env::temp_dir(), None);
    assert_eq!(
        status_of(router, "/api/data", None).await,
        StatusCode::OK,
        "没配 token 时中间件不该拦任何请求"
    );
}

/// token 只盖 /api/*：静态文件没有数据，不该被 401 挡住（页面本身要能加载，
/// 令牌由页面 JS 带在请求头里）。
#[tokio::test]
async fn static_files_are_not_gated() {
    let (app, _fake) = fake();
    let dir = tempfile::tempdir().expect("临时目录");
    let router = router(
        Arc::new(app),
        dir.path().to_path_buf(),
        Some("s3cret".to_string()),
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

/// 绑定校验：非回环 + 没 token = 拒绝；其余组合都有出路。
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

/// 常量时间比较：等、不等、长度不同，语义与 == 一致。
#[test]
fn constant_time_comparison_agrees_with_equality() {
    assert!(constant_time_eq(b"abc", b"abc"));
    assert!(!constant_time_eq(b"abc", b"abd"));
    assert!(!constant_time_eq(b"abc", b"abcd"));
    assert!(constant_time_eq(b"", b""));
}
