//! User-identity token tests: storage discipline and the refresh path (the part that runs
//! unattended). The browser round trip itself is a human step — it is exercised by hand,
//! and its failure mode (Feishu rejects the exchange) is covered here through `exchange`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::config::FeishuConfig;
use super::feishu_oauth::{self, UserToken};

fn config(base_url: String) -> FeishuConfig {
    FeishuConfig {
        app_id: "cli_test".to_string(),
        app_secret: "env:JOY_FEISHU_OAUTH_SECRET".to_string(),
        identity: Some("user".to_string()),
        receive_id_type: "open_id".to_string(),
        allow_recipients: vec!["ou_alice".to_string()],
        live: true,
        base_url,
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("时钟")
        .as_secs() as i64
}

fn write_token(home: &std::path::Path, token: &UserToken) {
    let path = home.join("connector-auth").join("feishu.json");
    std::fs::create_dir_all(path.parent().expect("父目录")).expect("建目录");
    std::fs::write(&path, serde_json::to_vec(token).expect("序列化")).expect("写");
}

/// Fake token endpoint: answers the refresh grant and records what it saw.
async fn fake_token_endpoint(
    code: i64,
) -> (String, Arc<AtomicUsize>, Arc<std::sync::Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("监听");
    let port = listener.local_addr().expect("地址").port();
    let calls = Arc::new(AtomicUsize::new(0));
    let bodies: Arc<std::sync::Mutex<Vec<Value>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (count, seen) = (calls.clone(), bodies.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let count = count.clone();
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                let header_end = loop {
                    let Ok(n) = stream.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let length: usize = head
                    .lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
                    .and_then(|line| line[15..].trim().parse().ok())
                    .unwrap_or(0);
                let mut body = buf[header_end + 4..].to_vec();
                while body.len() < length {
                    let Ok(n) = stream.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..n]);
                }
                count.fetch_add(1, Ordering::SeqCst);
                seen.lock()
                    .expect("锁")
                    .push(serde_json::from_slice(&body).unwrap_or(Value::Null));

                let reply = if code == 0 {
                    json!({"code": 0, "access_token": "u-new", "refresh_token": "r-new", "expires_in": 7200})
                } else {
                    json!({"code": code, "error_description": "redirect_uri mismatch"})
                }
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                    reply.len(),
                    reply
                );
                stream.write_all(response.as_bytes()).await.ok();
            });
        }
    });
    (format!("http://127.0.0.1:{port}"), calls, bodies)
}

#[tokio::test]
async fn a_fresh_token_is_used_without_asking_anyone() {
    let dir = tempdir().expect("临时目录");
    write_token(
        dir.path(),
        &UserToken {
            access_token: "u-valid".to_string(),
            refresh_token: "r-old".to_string(),
            expires_at: now() + 3600,
        },
    );
    // Port 9 is unreachable: reaching the network here would fail the test.
    let token = feishu_oauth::access_token(&config("http://127.0.0.1:9".to_string()), dir.path())
        .await
        .expect("有效 token 该直接用");
    assert_eq!(token, "u-valid");
}

#[tokio::test]
async fn an_expired_token_is_refreshed_and_saved() {
    let dir = tempdir().expect("临时目录");
    std::env::set_var("JOY_FEISHU_OAUTH_SECRET", "s3cret");
    write_token(
        dir.path(),
        &UserToken {
            access_token: "u-old".to_string(),
            refresh_token: "r-old".to_string(),
            expires_at: now() - 10,
        },
    );
    let (base, calls, bodies) = fake_token_endpoint(0).await;

    let token = feishu_oauth::access_token(&config(base), dir.path())
        .await
        .expect("该续期成功");
    assert_eq!(token, "u-new");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let body = bodies.lock().expect("锁")[0].clone();
    assert_eq!(body["grant_type"], json!("refresh_token"));
    assert_eq!(body["refresh_token"], json!("r-old"));
    assert_eq!(body["client_id"], json!("cli_test"));

    let saved = feishu_oauth::load(dir.path()).expect("该存下来了");
    assert_eq!(saved.access_token, "u-new");
    assert_eq!(saved.refresh_token, "r-new");
    assert!(saved.expires_at > now(), "新过期时间该在未来");
}

#[tokio::test]
async fn a_rejected_refresh_names_the_reauth_command() {
    let dir = tempdir().expect("临时目录");
    std::env::set_var("JOY_FEISHU_OAUTH_SECRET", "s3cret");
    write_token(
        dir.path(),
        &UserToken {
            access_token: "u-old".to_string(),
            refresh_token: "r-old".to_string(),
            expires_at: now() - 10,
        },
    );
    let (base, _calls, _bodies) = fake_token_endpoint(20005).await;
    let error = feishu_oauth::access_token(&config(base), dir.path())
        .await
        .expect_err("续期被拒该失败");
    let text = error.to_string();
    assert!(text.contains("重新跑"), "要告诉人下一步做什么：{text}");
}

#[tokio::test]
async fn no_login_is_an_error_with_the_login_command() {
    let dir = tempdir().expect("临时目录");
    let error = feishu_oauth::access_token(&config("http://127.0.0.1:9".to_string()), dir.path())
        .await
        .expect_err("没登录就该失败");
    assert!(error.to_string().contains("feishu login"), "{error}");
}

#[tokio::test]
async fn logout_removes_the_token_and_says_whether_there_was_one() {
    let dir = tempdir().expect("临时目录");
    write_token(
        dir.path(),
        &UserToken {
            access_token: "u".to_string(),
            refresh_token: "r".to_string(),
            expires_at: now() + 10,
        },
    );
    assert!(feishu_oauth::logout(dir.path()).expect("删"));
    assert!(feishu_oauth::status(dir.path()).is_none());
    assert!(!feishu_oauth::logout(dir.path()).expect("再删"));
}

#[cfg(unix)]
#[tokio::test]
async fn the_token_file_is_written_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir().expect("临时目录");
    feishu_oauth::save(
        dir.path(),
        &UserToken {
            access_token: "u".to_string(),
            refresh_token: "r".to_string(),
            expires_at: 0,
        },
    )
    .expect("写 token");
    let path = dir.path().join("connector-auth").join("feishu.json");
    assert_eq!(
        std::fs::metadata(path)
            .expect("元数据")
            .permissions()
            .mode()
            & 0o777,
        0o600,
        "token 文件必须只有本人可读"
    );
}
