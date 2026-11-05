//! `email_send` end to end against a **real local SMTP server** (scripted, no network):
//! drafts, the allowlist, the live switch, the approval card and the wire dialogue.
//!
//! The fake server is the same discipline as `smoke-gateway.sh`: stub the far end only;
//! everything between the tool and the socket is the real code.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use joyczl_tools::approval::{ApprovalBroker, ApprovalFut, ApprovalRequest};
use joyczl_tools::{Tool, ToolCtx};
use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use super::config::EmailConfig;
use super::email_send_tool;

// ---- scaffolding ---------------------------------------------------------------------

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    async fn ctx(&self, approval: Option<Arc<dyn ApprovalBroker>>) -> ToolCtx {
        let home = self.dir.path().to_path_buf();
        let pool = joyczl_state_open(&home).await;
        ToolCtx {
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
        }
    }
}

async fn joyczl_state_open(home: &std::path::Path) -> sqlx::SqlitePool {
    joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库")
}

fn config(port: u16, live: bool) -> EmailConfig {
    EmailConfig {
        imap: None,
        smtp_host: "127.0.0.1".to_string(),
        smtp_port: port,
        from: "joy@corp.com".to_string(),
        username: None,
        secret: None,
        allow_recipients: vec!["@corp.com".to_string()],
        live,
    }
}

async fn call(tool: &Tool, ctx: ToolCtx, args: Value) -> anyhow::Result<String> {
    (tool.handler)(ctx, args).await
}

/// A broker that records the cards and answers by script.
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
        self.seen.lock().expect("锁").push(request);
        let answer = self.answer;
        Box::pin(async move { answer })
    }
}

/// The far end: a scripted SMTP server that records the DATA payload.
struct FakeSmtp {
    port: u16,
    data: Arc<Mutex<String>>,
    auth_seen: Arc<AtomicBool>,
}

async fn fake_smtp() -> FakeSmtp {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("监听");
    let port = listener.local_addr().expect("地址").port();
    let data = Arc::new(Mutex::new(String::new()));
    let auth_seen = Arc::new(AtomicBool::new(false));
    let (data_for_task, auth_for_task) = (data.clone(), auth_seen.clone());

    tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        converse(stream, data_for_task, auth_for_task).await;
    });

    FakeSmtp {
        port,
        data,
        auth_seen,
    }
}

async fn converse(mut stream: TcpStream, data: Arc<Mutex<String>>, auth_seen: Arc<AtomicBool>) {
    let (read, mut write) = stream.split();
    let mut reader = BufReader::new(read);
    write.write_all(b"220 fake-smpt ready\r\n").await.ok();

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        let line = line.trim_end();
        let reply: &[u8] = if line.starts_with("EHLO") {
            b"250-fake\r\n250-AUTH PLAIN\r\n250 OK\r\n"
        } else if line.starts_with("AUTH PLAIN") {
            auth_seen.store(true, Ordering::SeqCst);
            b"235 accepted\r\n"
        } else if line == "DATA" {
            b"354 go ahead\r\n"
        } else if line == "QUIT" {
            write.write_all(b"221 bye\r\n").await.ok();
            return;
        } else {
            b"250 ok\r\n"
        };
        write.write_all(reply).await.ok();

        if line == "DATA" {
            // Read until the end-of-data marker; keep the payload for assertions.
            let mut payload = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                let n = match reader.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                payload.extend_from_slice(&buf[..n]);
                if payload.windows(3).any(|w| w == b"\r\n.") {
                    break;
                }
            }
            *data.lock().expect("锁") = String::from_utf8_lossy(&payload).to_string();
            write.write_all(b"250 queued\r\n").await.ok();
        }
    }
}

// ---- drafts --------------------------------------------------------------------------

#[tokio::test]
async fn without_live_a_send_is_only_a_draft() {
    let fixture = Fixture {
        dir: tempdir().expect("临时目录"),
    };
    let tool = email_send_tool(config(1, false));
    let ctx = fixture.ctx(None).await;

    let reply = call(
        &tool,
        ctx,
        json!({"to": "alice@corp.com", "subject": "周报", "body": "这周干得不错"}),
    )
    .await
    .expect("草稿该成功");
    assert!(reply.contains("草稿"), "{reply}");
    assert!(reply.contains("不会真的发"), "{reply}");

    let outbox = fixture.dir.path().join("outbox");
    let mut entries = std::fs::read_dir(&outbox).expect("outbox 目录");
    let entry = entries.next().expect("至少一份草稿").expect("读");
    let text = std::fs::read_to_string(entry.path()).expect("读草稿");
    assert!(text.contains("Subject: 周报"), "{text}");
    assert!(text.contains("这周干得不错"), "{text}");
}

#[tokio::test]
async fn dry_run_overrides_live_and_asks_nothing() {
    let fixture = Fixture {
        dir: tempdir().expect("临时目录"),
    };
    let broker = Fake::answering(true);
    let tool = email_send_tool(config(1, true));
    let ctx = fixture.ctx(Some(broker.clone())).await;

    let reply = call(
        &tool,
        ctx,
        json!({"to": "alice@corp.com", "subject": "s", "body": "b", "dry_run": true}),
    )
    .await
    .expect("显式 dry_run 该成功");
    assert!(reply.contains("dry_run"), "{reply}");
    assert!(
        broker.seen.lock().expect("锁").is_empty(),
        "留草稿不是对外动作，不该弹审批"
    );
}

// ---- the allowlist boundary ----------------------------------------------------------

#[tokio::test]
async fn a_recipient_outside_the_allowlist_stops_before_any_network() {
    let fixture = Fixture {
        dir: tempdir().expect("临时目录"),
    };
    let tool = email_send_tool(config(1, false));
    let ctx = fixture.ctx(None).await;

    let error = call(
        &tool,
        ctx,
        json!({"to": "mallory@elsewhere.io", "subject": "s", "body": "b"}),
    )
    .await
    .expect_err("白名单外必须拒绝");
    let text = error.to_string();
    assert!(text.contains("mallory@elsewhere.io"), "{text}");
    assert!(text.contains("allow_recipients"), "要说清去哪改：{text}");
}

// ---- live: approval + the wire dialogue ----------------------------------------------

#[tokio::test]
async fn a_denied_approval_means_nothing_is_sent() {
    let fixture = Fixture {
        dir: tempdir().expect("临时目录"),
    };
    let broker = Fake::answering(false);
    let tool = email_send_tool(config(9, true)); // port 9 = discard: nothing listens
    let ctx = fixture.ctx(Some(broker.clone())).await;

    let error = call(
        &tool,
        ctx,
        json!({"to": "alice@corp.com", "subject": "s", "body": "b"}),
    )
    .await
    .expect_err("没批准就不能发");
    assert!(error.to_string().contains("没有被批准"), "{error}");

    let seen = broker.seen.lock().expect("锁");
    assert_eq!(seen.len(), 1, "每次实际发送问一次");
    assert!(
        seen[0].args_preview.contains("alice@corp.com"),
        "卡片要写清收件人"
    );
    assert!(seen[0].args_preview.contains("s"), "卡片要带上主题");
}

#[tokio::test]
async fn an_approved_send_reaches_the_smtp_server() {
    let fixture = Fixture {
        dir: tempdir().expect("临时目录"),
    };
    let server = fake_smtp().await;
    let broker = Fake::answering(true);
    let tool = email_send_tool(config(server.port, true));
    let ctx = fixture.ctx(Some(broker.clone())).await;

    let reply = call(
        &tool,
        ctx,
        json!({"to": "alice@corp.com", "subject": "你好", "body": "测试正文"}),
    )
    .await
    .expect("批准后该发成功");
    assert!(reply.contains("已通过"), "{reply}");

    // The payload is MIME base64: decode the body back and compare.
    use base64::Engine;
    let raw = server.data.lock().expect("锁").clone();
    assert!(raw.contains("Subject: =?UTF-8?B?"), "中文主题要编码：{raw}");
    assert!(raw.contains("To: alice@corp.com"), "{raw}");
    let body_b64 = raw
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or_default()
        .replace("\r\n", "")
        .trim_end_matches('.')
        .to_string();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(body_b64.trim())
        .expect("正文是 base64");
    assert_eq!(String::from_utf8_lossy(&decoded), "测试正文");
    assert!(
        !server.auth_seen.load(Ordering::SeqCst),
        "没配用户名就不该发 AUTH"
    );
}

#[tokio::test]
async fn a_missing_password_is_reported_before_the_human_is_asked() {
    let fixture = Fixture {
        dir: tempdir().expect("临时目录"),
    };
    let broker = Fake::answering(true);
    let mut cfg = config(9, true);
    cfg.username = Some("joy@corp.com".to_string());
    cfg.secret = Some("env:JOY_SMTP_PASSWORD_NOT_SET".to_string());
    let tool = email_send_tool(cfg);
    let ctx = fixture.ctx(Some(broker.clone())).await;

    let error = call(
        &tool,
        ctx,
        json!({"to": "alice@corp.com", "subject": "s", "body": "b"}),
    )
    .await
    .expect_err("没有密码就该失败");
    assert!(error.to_string().contains("密码"), "{error}");
    assert!(
        broker.seen.lock().expect("锁").is_empty(),
        "参数不全就别浪费那次点击"
    );
}
