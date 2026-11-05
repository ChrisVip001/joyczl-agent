//! Auth tests: v1→v2 migration, register/login round trips, seat limits, the
//! first-user gate, and the HTTP face (no-token reachability, wrong-password 401,
//! single-user 400).
//!
//! The KDF cost is real (210k iterations) — each test pays a few hashes, not hundreds.
//! The guarded-API happy path (token → /api/data → 200) needs a real app-server child;
//! that chain is smoke-covered (smoke.sh's hub section walks it end to end).

use super::client::client_tests::fake;
use super::hub::{AuthError, Hub, Users};
use super::router_tests::auto_respond;
use axum::body::Body;
use axum::http::StatusCode;
use axum::Router;
use tower::ServiceExt;

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().expect("临时目录")
}

fn hub_in(dir: &tempfile::TempDir, max_open: usize) -> Hub {
    Hub::open_with(dir.path(), max_open, 0).expect("开 hub")
}

/// POST one JSON body, return (status, parsed body).
async fn post_json(router: Router, path: &str, body: &str) -> (StatusCode, serde_json::Value) {
    let response = router
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("请求体"),
        )
        .await
        .expect("路由响应");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("响应体");
    let parsed = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, parsed)
}

/// The flat v1 file migrates in memory: the token becomes device "cli", access survives,
/// and the next save writes the v2 shape.
#[test]
fn the_v1_table_migrates_and_survives() {
    let dir = temp();
    let path = dir.path().join("users.json");
    std::fs::write(&path, r#"{"alice":"t-a"}"#).expect("写 v1 格式");

    let users = Users::load(&path).expect("v1 读进来");
    assert_eq!(
        users.verify("t-a").as_deref(),
        Some("alice"),
        "旧 token 继续有效"
    );
    users.save(&path).expect("回存");
    let text = std::fs::read_to_string(&path).expect("读文件");
    assert!(text.contains("\"users\""), "存盘后应是 v2 形状");
    let loaded = Users::load(&path).expect("v2 读回");
    assert_eq!(loaded.verify("t-a").as_deref(), Some("alice"));
}

/// Register → the token verifies immediately, the device pairs; same device on login
/// returns the same token (登录即配对), a second device gets its own.
#[test]
fn register_then_login_round_trips() {
    let dir = temp();
    let hub = hub_in(&dir, 8);

    let token = hub
        .register("alice", "hunter2-secret", "我的手机")
        .expect("注册成功");
    assert_eq!(hub.user_count(), 1);
    assert_eq!(
        hub.resolve(Some(&format!("Bearer {token}"))).as_deref(),
        Some("alice"),
        "注册发的 token 立即可用"
    );

    let again = hub
        .login("alice", "hunter2-secret", "我的手机")
        .expect("同设备登录");
    assert_eq!(again, token, "同设备同 token");

    let other = hub
        .login("alice", "hunter2-secret", "work-laptop")
        .expect("新设备");
    assert_ne!(other, token, "新设备拿新 token");
    assert_eq!(hub.user_count(), 1, "登录不加座位，只添设备");
}

/// Wrong password, unknown user (same verdict — no user enumeration), duplicate
/// registration, and input rules all refuse.
#[test]
fn bad_credentials_and_bad_input_refuse() {
    let dir = temp();
    let hub = hub_in(&dir, 8);
    hub.register("alice", "hunter2-secret", "phone")
        .expect("先注册");

    assert!(matches!(
        hub.login("alice", "wrong-password", "phone"),
        Err(AuthError::Invalid(_))
    ));
    assert!(
        matches!(
            hub.login("nobody", "hunter2-secret", "phone"),
            Err(AuthError::Invalid(_))
        ),
        "未知用户与错密码同词同响应"
    );
    assert!(matches!(
        hub.register("alice", "hunter2-secret", "phone"),
        Err(AuthError::Exists)
    ));
    assert!(matches!(
        hub.register("Bad Name", "hunter2-secret", "phone"),
        Err(AuthError::BadInput(_))
    ));
    assert!(matches!(
        hub.register("bob", "short", "phone"),
        Err(AuthError::BadInput(_))
    ));
}

/// `max_open` doubles as the seat count: a full table closes registration.
#[test]
fn the_seat_cap_closes_registration() {
    let dir = temp();
    let hub = hub_in(&dir, 1);
    hub.register("alice", "hunter2-secret", "phone")
        .expect("占最后一个座位");
    assert!(matches!(
        hub.register("bob", "hunter2-secret", "phone"),
        Err(AuthError::Full)
    ));
}

/// A fresh hub on a public bind refuses the first self-registration (the owner must be
/// created from loopback or via adduser); once any user exists, seats decide.
#[test]
fn the_first_user_gate_closes_public_hubs() {
    let dir = temp();
    let hub = hub_in(&dir, 8);
    hub.permit_first_registration(false);
    assert!(matches!(
        hub.register("alice", "hunter2-secret", "phone"),
        Err(AuthError::Closed)
    ));

    // CLI 路径不受门控管：adduser 建出主人后，后续注册按座位走。
    let mut users = Users::load(&dir.path().join("users.json")).expect("空表");
    users.add("owner", "t-owner");
    users.save(&dir.path().join("users.json")).expect("存盘");
    let hub = Hub::open_with(dir.path(), 8, 0).expect("重开");
    hub.permit_first_registration(false);
    hub.register("alice", "hunter2-secret", "phone")
        .expect("有主人后注册照常");
}

/// The HTTP face. Register needs no Authorization header; wrong password is 401;
/// single-user mode says there is nothing to log into.
#[tokio::test]
async fn the_http_face_answers_without_a_token() {
    let dir = temp();
    let hub = std::sync::Arc::new(hub_in(&dir, 8));
    let router = super::router(
        super::Gate::hub(std::sync::Arc::clone(&hub)),
        std::env::temp_dir(),
    );

    let (status, body) = post_json(
        router.clone(),
        "/api/auth/register",
        r#"{"username":"alice","password":"hunter2-secret","device":"phone"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "注册免鉴权可达");
    assert_eq!(body["user"], "alice");
    assert!(body["token"].is_string());
    let token = body["token"].as_str().expect("token").to_string();
    assert_eq!(
        hub.resolve(Some(&format!("Bearer {token}"))).as_deref(),
        Some("alice"),
        "HTTP 注册的 token 直接可解析"
    );

    let (status, body) = post_json(
        router.clone(),
        "/api/auth/login",
        r#"{"username":"alice","password":"no-such-thing","device":"phone"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body["message"].is_string(), "错误走 code/message 形状");

    let (status, _) = post_json(
        router.clone(),
        "/api/auth/register",
        r#"{"username":"alice","password":"hunter2-secret","device":"phone"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "重名注册 409");
}

/// Single-user mode: the auth endpoints exist but answer 400 — nothing to log into.
#[tokio::test]
async fn single_user_mode_has_nothing_to_log_into() {
    let (app, fake) = fake();
    auto_respond(fake);
    let router = super::router(
        super::Gate::single(std::sync::Arc::new(app), None),
        std::env::temp_dir(),
    );
    let (status, _) = post_json(
        router,
        "/api/auth/login",
        r#"{"username":"a","password":"hunter2-secret","device":"d"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
