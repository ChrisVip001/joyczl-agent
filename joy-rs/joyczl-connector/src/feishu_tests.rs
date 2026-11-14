//! `feishu_send` against a scripted Feishu endpoint (a real local HTTP server):
//! the token cache, the message body shape, and the same three fences as email.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use joyczl_tools::approval::{ApprovalBroker, ApprovalFut, ApprovalRequest};
use joyczl_tools::{Tool, ToolCtx};
use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::config::FeishuConfig;
use super::feishu_send_tool;

// ---- scaffolding ---------------------------------------------------------------------

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    async fn ctx(&self, approval: Option<Arc<dyn ApprovalBroker>>) -> ToolCtx {
        let home = self.dir.path().to_path_buf();
        let pool = joyczl_state::open(&home.join("state.db"))
            .await
            .expect("open the store");
        ToolCtx {
            on_uncertain: None,
            resource_lease: None,
            task_run_id: None,
            activate: None,
            embedder: None,
            cwd: None,
            approval,
            jobs: None,
            hooks: None,
            turn_limits: Arc::new(joyczl_tools::web::TurnLimits::default()),
            session_id: "test".to_string(),
            facts: joyczl_state::Facts::new(pool.clone()),
            episodes: joyczl_state::Episodes::new(pool.clone()),
            chat: joyczl_state::Chat::new(pool.clone()),
            calendar: joyczl_state::Calendar::new(pool),
            home,
            on_delta: None,
        }
    }
}

fn config(base_url: String, live: bool) -> FeishuConfig {
    FeishuConfig {
        app_id: "cli_test".to_string(),
        identity: None,
        app_secret: "env:JOY_FEISHU_TEST_SECRET".to_string(),
        receive_id_type: "open_id".to_string(),
        allow_recipients: vec!["ou_alice".to_string()],
        live,
        base_url,
    }
}

async fn call(tool: &Tool, ctx: ToolCtx, args: Value) -> anyhow::Result<String> {
    (tool.handler)(ctx, args).await
}

struct Fake {
    answer: bool,
    seen: Mutex<Vec<ApprovalRequest>>,
}

impl Fake {
    fn answering(answer: bool) -> Arc<Self> {
        Arc::new(Self {
            answer,
            seen: Mutex::new(Vec::new()),
        })
    }
}

impl ApprovalBroker for Fake {
    fn request(&self, request: ApprovalRequest) -> ApprovalFut {
        self.seen.lock().expect("lock").push(request);
        let answer = self.answer;
        Box::pin(async move { answer })
    }
}

/// One recorded message call: the Authorization header and the JSON body.
type MessageCall = (Option<String>, Value);

/// A scripted Feishu: accepts requests, answers by path, records what it saw.
struct FakeFeishu {
    base_url: String,
    token_requests: Arc<AtomicUsize>,
    messages: Arc<Mutex<Vec<MessageCall>>>,
}

async fn fake_feishu(token_code: i64, message_code: i64) -> FakeFeishu {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listen");
    let port = listener.local_addr().expect("the address").port();
    let token_requests = Arc::new(AtomicUsize::new(0));
    let messages: Arc<Mutex<Vec<MessageCall>>> = Arc::new(Mutex::new(Vec::new()));
    let (tokens, msgs) = (token_requests.clone(), messages.clone());

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let (tokens, msgs) = (tokens.clone(), msgs.clone());
            tokio::spawn(async move {
                let Some(request) = read_request(&mut stream).await else {
                    return;
                };
                let (path, auth, body) = request;
                let payload = if path.contains("tenant_access_token") {
                    tokens.fetch_add(1, Ordering::SeqCst);
                    json!({"code": token_code, "msg": if token_code == 0 { "ok" } else { "app not found" },
                           "tenant_access_token": "t-fake", "expire": 7200})
                } else if path.contains("/im/v1/messages") {
                    msgs.lock().expect("lock").push((auth, body));
                    json!({"code": message_code,
                           "msg": if message_code == 0 { "ok" } else { "invalid receive_id" },
                           "data": {"message_id": "om_fake"}})
                } else {
                    json!({"code": 404, "msg": "not found"})
                };
                let text = payload.to_string();
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                    text.len(),
                    text
                );
                stream.write_all(reply.as_bytes()).await.ok();
            });
        }
    });

    FakeFeishu {
        base_url: format!("http://127.0.0.1:{port}"),
        token_requests,
        messages,
    }
}

/// Minimal HTTP/1.1 request reader: one request, headers, Content-Length body.
async fn read_request(
    stream: &mut tokio::net::TcpStream,
) -> Option<(String, Option<String>, Value)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let path = head.lines().next()?.split_whitespace().nth(1)?.to_string();
    let auth = head
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("authorization:"))
        .map(|line| line[14..].trim().to_string());
    let length: usize = head
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|line| line[15..].trim().parse().ok())
        .unwrap_or(0);
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
    Some((path, auth, body))
}

// ---- the fences ----------------------------------------------------------------------

#[tokio::test]
async fn a_dry_run_is_only_a_draft() {
    let fixture = Fixture {
        dir: tempdir().expect("temp dir"),
    };
    let tool = feishu_send_tool(config("http://127.0.0.1:9".to_string(), false));
    let ctx = fixture.ctx(None).await;

    let reply = call(
        &tool,
        ctx,
        json!({"to": "ou_alice", "text": "see you tomorrow"}),
    )
    .await
    .expect("the draft should succeed");
    assert!(reply.contains("draft"), "{reply}");
    let outbox = fixture.dir.path().join("outbox");
    let entry = std::fs::read_dir(&outbox)
        .expect("outbox")
        .next()
        .expect("there is a draft")
        .expect("read");
    let text = std::fs::read_to_string(entry.path()).expect("read the draft");
    assert!(text.contains("To: ou_alice (open_id)"), "{text}");
    assert!(text.contains("see you tomorrow"), "{text}");
}

#[tokio::test]
async fn an_unknown_recipient_is_refused() {
    let fixture = Fixture {
        dir: tempdir().expect("temp dir"),
    };
    let tool = feishu_send_tool(config("http://127.0.0.1:9".to_string(), false));
    let ctx = fixture.ctx(None).await;
    let error = call(&tool, ctx, json!({"to": "ou_mallory", "text": "hi"}))
        .await
        .expect_err("anything outside the whitelist must be refused");
    assert!(error.to_string().contains("ou_mallory"), "{error}");
    assert!(error.to_string().contains("allow_recipients"), "{error}");
}

#[tokio::test]
async fn a_denied_approval_never_touches_the_network() {
    let fixture = Fixture {
        dir: tempdir().expect("temp dir"),
    };
    std::env::set_var("JOY_FEISHU_TEST_SECRET", "s3cret");
    let server = fake_feishu(0, 0).await;
    let broker = Fake::answering(false);
    let tool = feishu_send_tool(config(server.base_url.clone(), true));
    let ctx = fixture.ctx(Some(broker.clone())).await;

    let error = call(&tool, ctx, json!({"to": "ou_alice", "text": "hi"}))
        .await
        .expect_err("no approval, nothing sent");
    assert!(error.to_string().contains("not approved"), "{error}");
    assert_eq!(broker.seen.lock().expect("lock").len(), 1);
    assert!(
        broker.seen.lock().expect("lock")[0]
            .args_preview
            .contains("ou_alice"),
        "the card has to say who it goes to"
    );
    assert_eq!(
        server.token_requests.load(Ordering::SeqCst),
        0,
        "after a refusal it should not even go asking for a token"
    );
}

#[tokio::test]
async fn an_approved_send_carries_a_tenant_token_and_a_text_message() {
    let fixture = Fixture {
        dir: tempdir().expect("temp dir"),
    };
    std::env::set_var("JOY_FEISHU_TEST_SECRET", "s3cret");
    let server = fake_feishu(0, 0).await;
    let broker = Fake::answering(true);
    let tool = feishu_send_tool(config(server.base_url.clone(), true));
    let ctx = fixture.ctx(Some(broker)).await;

    let reply = call(
        &tool,
        ctx,
        json!({"to": "ou_alice", "text": "the deployment is done"}),
    )
    .await
    .expect("after approval it should send");
    assert!(reply.contains("om_fake"), "{reply}");

    let messages = server.messages.lock().expect("lock");
    assert_eq!(messages.len(), 1);
    let (auth, body) = &messages[0];
    assert_eq!(
        auth.as_deref(),
        Some("Bearer t-fake"),
        "the message has to carry the app token"
    );
    assert_eq!(body["receive_id"], json!("ou_alice"));
    assert_eq!(body["msg_type"], json!("text"));
    // Feishu wants `content` as a JSON *string*, not an object.
    let content: Value =
        serde_json::from_str(body["content"].as_str().expect("content is a string"))
            .expect("content holds JSON");
    assert_eq!(content["text"], json!("the deployment is done"));
}

#[tokio::test]
async fn the_token_is_fetched_once_and_reused() {
    let fixture = Fixture {
        dir: tempdir().expect("temp dir"),
    };
    std::env::set_var("JOY_FEISHU_TEST_SECRET", "s3cret");
    let server = fake_feishu(0, 0).await;
    let broker = Fake::answering(true);
    let tool = feishu_send_tool(config(server.base_url.clone(), true));

    for text in ["the first one", "the second one"] {
        let ctx = fixture.ctx(Some(broker.clone())).await;
        call(&tool, ctx, json!({"to": "ou_alice", "text": text}))
            .await
            .expect("it should send");
    }
    assert_eq!(
        server.token_requests.load(Ordering::SeqCst),
        1,
        "within two hours the token is reused (in-process cache)"
    );
    assert_eq!(server.messages.lock().expect("lock").len(), 2);
}

#[tokio::test]
async fn feishu_s_own_error_words_are_passed_through() {
    let fixture = Fixture {
        dir: tempdir().expect("temp dir"),
    };
    std::env::set_var("JOY_FEISHU_TEST_SECRET", "s3cret");
    let server = fake_feishu(0, 99991663).await;
    let broker = Fake::answering(true);
    let tool = feishu_send_tool(config(server.base_url.clone(), true));
    let ctx = fixture.ctx(Some(broker)).await;

    let error = call(&tool, ctx, json!({"to": "ou_alice", "text": "hi"}))
        .await
        .expect_err("a Feishu error should fail");
    let text = error.to_string();
    assert!(text.contains("99991663"), "{text}");
    assert!(
        text.contains("invalid receive_id"),
        "it has to carry Feishu's own words: {text}"
    );
}

#[tokio::test]
async fn a_missing_secret_is_reported_before_the_human_is_asked() {
    let fixture = Fixture {
        dir: tempdir().expect("temp dir"),
    };
    let broker = Fake::answering(true);
    std::env::remove_var("JOY_FEISHU_TEST_SECRET_MISSING");
    let mut cfg = config("http://127.0.0.1:9".to_string(), true);
    cfg.app_secret = "env:JOY_FEISHU_TEST_SECRET_MISSING".to_string();
    let tool = feishu_send_tool(cfg);
    let ctx = fixture.ctx(Some(broker.clone())).await;

    let error = call(&tool, ctx, json!({"to": "ou_alice", "text": "hi"}))
        .await
        .expect_err("no secret means failure");
    assert!(error.to_string().contains("app_secret"), "{error}");
    assert!(
        broker.seen.lock().expect("lock").is_empty(),
        "do not waste that click"
    );
}
