//! Transport-layer tests. The fake server is just two pipes in memory — no real process,
//! no real disk: the only thing to verify here is "how frames go in and out", so test that.

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::io::{BufReader, DuplexStream, ReadHalf, WriteHalf};

use super::*;

/// The other end of the fake server: reads requests, writes answers and notifications.
pub(crate) struct Fake {
    requests: BufReader<ReadHalf<DuplexStream>>,
    to_client: WriteHalf<DuplexStream>,
}

/// One in-memory connection. Left side is our client, right side a fake app-server.
pub(crate) fn fake() -> (AppServer, Fake) {
    let (ours, theirs) = tokio::io::duplex(8192);
    let (reader, writer) = tokio::io::split(ours);
    let (theirs_read, theirs_write) = tokio::io::split(theirs);
    let app = AppServer::attach(std::env::temp_dir(), None, reader, writer);
    let fake = Fake {
        requests: BufReader::new(theirs_read),
        to_client: theirs_write,
    };
    (app, fake)
}

impl Fake {
    /// Receive one request (the line the client sent).
    pub(crate) async fn next_request(&mut self) -> Value {
        let mut line = String::new();
        self.requests
            .read_line(&mut line)
            .await
            .expect("读客户端发来的请求");
        serde_json::from_str(line.trim()).expect("请求是 JSON")
    }

    async fn send(&mut self, frame: Value) {
        let line = format!("{frame}\n");
        self.to_client
            .write_all(line.as_bytes())
            .await
            .expect("写给客户端");
    }

    pub(crate) async fn respond(&mut self, id: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await;
    }

    async fn fail(&mut self, id: &Value, code: i32, message: &str) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        }))
        .await;
    }

    async fn notify(&mut self, notification: Value) {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "turn/notification",
            "params": notification,
        }))
        .await;
    }

    /// Write a line verbatim, used to feed bad data.
    async fn raw(&mut self, text: &str) {
        let line = format!("{text}\n");
        self.to_client
            .write_all(line.as_bytes())
            .await
            .expect("写给客户端");
    }
}

/// Start a request in the background, so the test holds both ends of the pipe at once.
fn spawn_call(
    app: &Arc<AppServer>,
    method: &'static str,
) -> tokio::task::JoinHandle<Result<Value, ErrorObject>> {
    let app = Arc::clone(app);
    tokio::spawn(async move { app.call(method, json!({})).await })
}

#[tokio::test]
async fn 应答按_id_配对_倒着回来也对得上() {
    let (app, mut fake) = fake();
    let app = Arc::new(app);

    let first = spawn_call(&app, "memory/list");
    let second = spawn_call(&app, "session/list");

    let one = fake.next_request().await;
    let two = fake.next_request().await;
    assert_eq!(one["method"], "memory/list");
    assert_eq!(two["method"], "session/list");
    assert_ne!(one["id"], two["id"], "每条请求都该有自己的 id");

    // Answer out of order: the later arrival first. A mix-up would show right here.
    fake.respond(&two["id"], json!({"data": ["second"]})).await;
    fake.respond(&one["id"], json!({"data": ["first"]})).await;

    assert_eq!(second.await.unwrap().unwrap()["data"][0], "second");
    assert_eq!(first.await.unwrap().unwrap()["data"][0], "first");
}

#[tokio::test]
async fn 通知原样广播出来() {
    let (app, mut fake) = fake();
    // Subscribe before sending the request — the rule in real code; tests follow it.
    let mut events = app.subscribe();

    let call = spawn_call(&Arc::new(app), "turn/start");
    let request = fake.next_request().await;
    fake.notify(json!({"type": "textDelta", "turnId": "t1", "delta": "你"}))
        .await;
    fake.respond(&request["id"], json!({"turnId": "t1"})).await;

    match events.recv().await.expect("收到通知") {
        ServerNotification::TextDelta(delta) => {
            assert_eq!(delta.turn_id, "t1");
            assert_eq!(delta.delta, "你");
        }
        other => panic!("该是 textDelta，收到 {other:?}"),
    }
    assert!(call.await.unwrap().is_ok());
}

#[tokio::test]
async fn 错误带着原来的错误码传回去() {
    let (app, mut fake) = fake();
    let app = Arc::new(app);

    let call = spawn_call(&app, "turn/start");
    let request = fake.next_request().await;
    fake.fail(
        &request["id"],
        codes::PROVIDER_ERROR,
        "缺 API key：往 .env 里加 ANTHROPIC_API_KEY",
    )
    .await;

    let error = call.await.unwrap().expect_err("该是错误");
    // Passed back verbatim: the dashboard shows this sentence to the user directly.
    assert_eq!(error.code, codes::PROVIDER_ERROR);
    assert!(
        error.message.contains("ANTHROPIC_API_KEY"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn 读不懂的一行不会打断后面的帧() {
    let (app, mut fake) = fake();
    let app = Arc::new(app);

    let call = spawn_call(&app, "memory/list");
    let request = fake.next_request().await;
    fake.raw("这不是 JSON").await;
    fake.respond(&request["id"], json!({"data": []})).await;

    assert!(call.await.unwrap().is_ok(), "坏行之后应答照样要对上");
}

#[tokio::test]
async fn 服务端走了_挂着的请求会被叫醒() {
    let (app, mut fake) = fake();
    let app = Arc::new(app);

    let call = spawn_call(&app, "memory/list");
    let _ = fake.next_request().await;

    // Drop the far end of the pipe: the same as app-server exiting.
    drop(fake);

    let error = call
        .await
        .unwrap()
        .expect_err("连接断了就该报错，而不是永远挂着");
    assert_eq!(error.code, codes::INTERNAL_ERROR);
    assert!(error.message.contains("app-server"), "{}", error.message);
}
