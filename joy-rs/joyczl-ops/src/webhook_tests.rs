//! Inbound webhook fences, exercised through the real router.
//!
//! The full "runs a turn and answers" path needs a real app-server (smoke-dashboard.sh);
//! here the point is the door itself: who gets through, who gets 404, who gets 401 —
//! and that a hook that does get through hands over exactly one `turn/start`.

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::client::client_tests::{fake_at, Fake};
use crate::{router, Gate};

fn hooks_file(home: &Path, json: &str) {
    std::fs::write(home.join("webhooks.json"), json).expect("写 webhooks.json");
}

fn gate(home: &Path) -> (Gate, Fake) {
    let (app, fake) = fake_at(home.to_path_buf());
    (Gate::single(Arc::new(app), None), fake)
}

fn post(name: &str, secret: Option<&str>, body: &str) -> Request<Body> {
    let mut headers = Vec::new();
    if let Some(secret) = secret {
        headers.push(("x-joy-secret", secret));
    }
    post_with(name, &headers, body)
}

fn post_with(name: &str, headers: &[(&str, &str)], body: &str) -> Request<Body> {
    let mut builder =
        Request::post(format!("/api/hook/{name}")).header("content-type", "application/json");
    for (key, value) in headers {
        builder = builder.header(*key, *value);
    }
    builder.body(Body::from(body.to_string())).expect("请求")
}

async fn status_only(gate: Gate, request: Request<Body>) -> StatusCode {
    router(gate, std::env::temp_dir())
        .oneshot(request)
        .await
        .expect("响应")
        .status()
}

#[tokio::test]
async fn without_a_config_file_the_door_does_not_exist() {
    let home = tempfile::tempdir().expect("临时目录");
    let (gate, _fake) = gate(home.path());
    let status = status_only(gate, post("deploy", Some("anything"), "{}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "没配置就是没这扇门");
}

#[tokio::test]
async fn an_unknown_or_disabled_name_is_also_404() {
    let home = tempfile::tempdir().expect("临时目录");
    hooks_file(
        home.path(),
        r#"{"hooks": [
            {"name": "deploy", "secret": "s3cret", "prompt": "看 {{payload}}"},
            {"name": "off", "secret": "s3cret", "prompt": "看 {{payload}}", "enabled": false}
        ]}"#,
    );

    let (gate, _fake) = gate(home.path());
    assert_eq!(
        status_only(gate.clone(), post("nope", Some("s3cret"), "{}")).await,
        StatusCode::NOT_FOUND
    );
    // Switched off and never existed give the same answer: a hook cannot be probed for existence.
    assert_eq!(
        status_only(gate, post("off", Some("s3cret"), "{}")).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_wrong_or_missing_secret_is_401() {
    let home = tempfile::tempdir().expect("临时目录");
    hooks_file(
        home.path(),
        r#"{"hooks": [{"name": "deploy", "secret": "s3cret", "prompt": "看 {{payload}}"}]}"#,
    );

    let (gate, _fake) = gate(home.path());
    assert_eq!(
        status_only(gate.clone(), post("deploy", None, "{}")).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status_only(gate, post("deploy", Some("wrong"), "{}")).await,
        StatusCode::UNAUTHORIZED,
        "密钥不对就是 401，不是 404，也不能放行"
    );
}

#[tokio::test]
async fn a_secret_missing_from_the_env_refuses_every_request() {
    let home = tempfile::tempdir().expect("临时目录");
    hooks_file(
        home.path(),
        r#"{"hooks": [{"name": "deploy", "secret": "env:JOY_TEST_HOOK_NOT_SET", "prompt": "看 {{payload}}"}]}"#,
    );

    let (gate, _fake) = gate(home.path());
    // Even an empty provided secret must not match an unreadable expected one.
    assert_eq!(
        status_only(gate, post("deploy", Some(""), "{}")).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_good_secret_hands_over_exactly_one_turn() {
    let home = tempfile::tempdir().expect("临时目录");
    hooks_file(
        home.path(),
        r#"{"hooks": [{
            "name": "deploy",
            "secret": "s3cret",
            "prompt": "处理这个部署事件：\n{{payload}}",
            "tier": "fast"
        }]}"#,
    );

    let (gate, mut fake) = gate(home.path());
    let router = router(gate, std::env::temp_dir());
    let call = tokio::spawn(async move {
        router
            .oneshot(post("deploy", Some("s3cret"), "status=failed"))
            .await
            .expect("响应")
    });

    let request = fake.next_request().await;
    assert_eq!(request["method"], "turn/start");
    assert_eq!(request["params"]["sessionId"], "hook:deploy");
    assert_eq!(request["params"]["tier"], "fast", "档位要原样交给服务端");
    let message = request["params"]["message"].as_str().expect("消息是字符串");
    assert!(
        message.contains("status=failed"),
        "body 要进到 prompt 里：{message}"
    );

    // A failed turn comes back as the protocol's own error, not as a hung request.
    fake.fail(
        &request["id"],
        joyczl_protocol::codes::PROVIDER_ERROR,
        "模型没配好",
    )
    .await;
    let response = call.await.expect("任务不该 panic");
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("读 body");
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("-32000"), "错误码要原样给回调用方：{body}");
}

/// A signed hook: `X-Joy-Secret` alone is no longer enough, the timestamp must be fresh, and the
/// same signature twice is 409 — a replay, refused before any turn is spent.
#[tokio::test]
async fn a_signed_hook_refuses_stale_and_replayed_requests() {
    let home = tempfile::tempdir().expect("临时目录");
    hooks_file(
        home.path(),
        r#"{"hooks": [{
            "name": "signed-req",
            "secret": "sig-secret",
            "prompt": "看 {{payload}}",
            "signature": true
        }]}"#,
    );
    let body = r#"{"status":"failed"}"#;
    let now = chrono::Utc::now().timestamp();
    let sign = |epoch: i64| crate::webhooks::signature_of("sig-secret", &epoch.to_string(), body);

    let (door, _fake) = gate(home.path());
    // Old plain-secret habit no longer opens this door.
    assert_eq!(
        status_only(door.clone(), post("signed-req", Some("sig-secret"), body)).await,
        StatusCode::UNAUTHORIZED,
        "signature 模式不接受只有 X-Joy-Secret"
    );

    let stale = (now - 600).to_string();
    assert_eq!(
        status_only(
            door.clone(),
            post_with(
                "signed-req",
                &[
                    ("x-joy-timestamp", stale.as_str()),
                    ("x-joy-signature", sign(now - 600).as_str()),
                ],
                body,
            ),
        )
        .await,
        StatusCode::UNAUTHORIZED
    );

    // Fresh and correct: the turn starts (answered with an error so the pipe stays clean).
    let (gate, mut fake) = gate(home.path());
    let fresh = now.to_string();
    let signature = sign(now);
    let router = router(gate.clone(), std::env::temp_dir());
    let (sent_at, sent_signature) = (fresh.clone(), signature.clone());
    let call = tokio::spawn(async move {
        router
            .oneshot(post_with(
                "signed-req",
                &[
                    ("x-joy-timestamp", sent_at.as_str()),
                    ("x-joy-signature", sent_signature.as_str()),
                ],
                body,
            ))
            .await
            .expect("响应")
    });
    let request = fake.next_request().await;
    assert_eq!(request["method"], "turn/start");
    fake.fail(
        &request["id"],
        joyczl_protocol::codes::PROVIDER_ERROR,
        "模型没配好",
    )
    .await;
    assert_eq!(
        call.await.expect("不该 panic").status(),
        StatusCode::OK,
        "签名对就该放行"
    );

    // Same timestamp, same signature again: 409, and it never reaches the app-server.
    let replay = status_only(
        gate,
        post_with(
            "signed-req",
            &[
                ("x-joy-timestamp", fresh.as_str()),
                ("x-joy-signature", signature.as_str()),
            ],
            body,
        ),
    )
    .await;
    assert_eq!(replay, StatusCode::CONFLICT, "重放要当场拒掉");
}

/// The per-minute cap: the first accepted request runs, the second is 429 with `Retry-After`.
#[tokio::test]
async fn a_hook_can_cap_itself_per_minute() {
    let home = tempfile::tempdir().expect("临时目录");
    hooks_file(
        home.path(),
        r#"{"hooks": [{
            "name": "capped",
            "secret": "cap-secret",
            "prompt": "看 {{payload}}",
            "limit": 1
        }]}"#,
    );

    let (gate, mut fake) = gate(home.path());
    let first_router = router(gate.clone(), std::env::temp_dir());
    let call = tokio::spawn(async move {
        first_router
            .oneshot(post("capped", Some("cap-secret"), "{}"))
            .await
            .expect("响应")
    });
    let request = fake.next_request().await;
    fake.fail(
        &request["id"],
        joyczl_protocol::codes::PROVIDER_ERROR,
        "模型没配好",
    )
    .await;
    assert_eq!(call.await.expect("不该 panic").status(), StatusCode::OK);

    let response = router(gate, std::env::temp_dir())
        .oneshot(post("capped", Some("cap-secret"), "{}"))
        .await
        .expect("响应");
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        retry
            .parse::<u64>()
            .is_ok_and(|seconds| (1..=60).contains(&seconds)),
        "Retry-After 要给出等多久：{retry:?}"
    );
}

/// Async hooks answer `202` at once and push the answer to the callback afterwards.
#[tokio::test]
async fn an_async_hook_answers_202_and_pushes_the_result_to_its_callback() {
    // The "foreign system" the answer is pushed to: a real listener on an ephemeral port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑本地端口");
    let address = listener.local_addr().expect("端口");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let receiver = axum::Router::new().route(
        "/result",
        axum::routing::post(move |body: String| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(body);
                "ok"
            }
        }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(listener, receiver).await;
    });

    let home = tempfile::tempdir().expect("临时目录");
    hooks_file(
        home.path(),
        &format!(
            r#"{{"hooks": [{{
                "name": "async-hook",
                "secret": "async-secret",
                "prompt": "看 {{{{payload}}}}",
                "session": "hook:async-test",
                "callback": "http://{address}/result"
            }}]}}"#
        ),
    );

    let (gate, mut fake) = gate(home.path());
    let response = router(gate, std::env::temp_dir())
        .oneshot(post("async-hook", Some("async-secret"), "boom"))
        .await
        .expect("响应");
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "异步 hook 要立刻收下，而不是等这一轮跑完"
    );
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("读 body");
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("\"accepted\":true"), "{body}");
    assert!(body.contains("hook:async-test"), "{body}");

    // The turn runs in the background; its failure is delivered, not swallowed.
    let request = fake.next_request().await;
    assert_eq!(request["method"], "turn/start");
    fake.fail(
        &request["id"],
        joyczl_protocol::codes::PROVIDER_ERROR,
        "模型没配好",
    )
    .await;

    let pushed = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
        .await
        .expect("回调要送到")
        .expect("收到回调");
    let pushed: serde_json::Value = serde_json::from_str(&pushed).expect("回调是 JSON");
    assert_eq!(pushed["ok"], serde_json::json!(false));
    assert_eq!(pushed["hook"], serde_json::json!("async-hook"));
    assert_eq!(pushed["session"], serde_json::json!("hook:async-test"));
    assert!(
        pushed["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("模型没配好")),
        "失败原因要原样送到：{pushed}"
    );
}
