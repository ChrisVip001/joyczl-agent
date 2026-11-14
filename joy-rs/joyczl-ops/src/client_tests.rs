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
    fake_at(std::env::temp_dir())
}

/// Same fake, but the child "serves" the home the caller passes in — needed by tests that
/// read a config file from `<home>` (webhooks.json) instead of the shared temp dir.
pub(crate) fn fake_at(home: std::path::PathBuf) -> (AppServer, Fake) {
    let (ours, theirs) = tokio::io::duplex(8192);
    let (reader, writer) = tokio::io::split(ours);
    let (theirs_read, theirs_write) = tokio::io::split(theirs);
    let app = AppServer::attach(home, None, reader, writer);
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
            .expect("read the client's request");
        serde_json::from_str(line.trim()).expect("the request is JSON")
    }

    async fn send(&mut self, frame: Value) {
        let line = format!("{frame}\n");
        self.to_client
            .write_all(line.as_bytes())
            .await
            .expect("write to the client");
    }

    pub(crate) async fn respond(&mut self, id: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await;
    }

    pub(crate) async fn fail(&mut self, id: &Value, code: i32, message: &str) {
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
            .expect("write to the client");
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
async fn responses_pair_by_id_even_when_they_come_back_out_of_order() {
    let (app, mut fake) = fake();
    let app = Arc::new(app);

    let first = spawn_call(&app, "memory/list");
    let second = spawn_call(&app, "session/list");

    let one = fake.next_request().await;
    let two = fake.next_request().await;
    assert_eq!(one["method"], "memory/list");
    assert_eq!(two["method"], "session/list");
    assert_ne!(
        one["id"], two["id"],
        "every request has to carry its own id"
    );

    // Answer out of order: the later arrival first. A mix-up would show right here.
    fake.respond(&two["id"], json!({"data": ["second"]})).await;
    fake.respond(&one["id"], json!({"data": ["first"]})).await;

    assert_eq!(second.await.unwrap().unwrap()["data"][0], "second");
    assert_eq!(first.await.unwrap().unwrap()["data"][0], "first");
}

#[tokio::test]
async fn notifications_are_broadcast_unchanged() {
    let (app, mut fake) = fake();
    // Subscribe before sending the request — the rule in real code; tests follow it.
    let mut events = app.subscribe();

    let call = spawn_call(&Arc::new(app), "turn/start");
    let request = fake.next_request().await;
    fake.notify(json!({"type": "textDelta", "turnId": "t1", "delta": "you"}))
        .await;
    fake.respond(&request["id"], json!({"turnId": "t1"})).await;

    match events.recv().await.expect("the notice arrives") {
        ServerNotification::TextDelta(delta) => {
            assert_eq!(delta.turn_id, "t1");
            assert_eq!(delta.delta, "you");
        }
        other => panic!("this should be textDelta, got {other:?}"),
    }
    assert!(call.await.unwrap().is_ok());
}

#[tokio::test]
async fn an_error_carries_its_code_back() {
    let (app, mut fake) = fake();
    let app = Arc::new(app);

    let call = spawn_call(&app, "turn/start");
    let request = fake.next_request().await;
    fake.fail(
        &request["id"],
        codes::PROVIDER_ERROR,
        "no API key: add ANTHROPIC_API_KEY to .env",
    )
    .await;

    let error = call.await.unwrap().expect_err("it should be an error");
    // Passed back verbatim: the dashboard shows this sentence to the user directly.
    assert_eq!(error.code, codes::PROVIDER_ERROR);
    assert!(
        error.message.contains("ANTHROPIC_API_KEY"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn an_unreadable_line_does_not_break_the_frames_after_it() {
    let (app, mut fake) = fake();
    let app = Arc::new(app);

    let call = spawn_call(&app, "memory/list");
    let request = fake.next_request().await;
    fake.raw("this is not JSON").await;
    fake.respond(&request["id"], json!({"data": []})).await;

    assert!(
        call.await.unwrap().is_ok(),
        "after a bad line the responses still have to pair up"
    );
}

#[tokio::test]
async fn a_departed_server_wakes_the_pending_requests() {
    let (app, mut fake) = fake();
    let app = Arc::new(app);

    let call = spawn_call(&app, "memory/list");
    let _ = fake.next_request().await;

    // Drop the far end of the pipe: the same as app-server exiting.
    drop(fake);

    let error = call
        .await
        .unwrap()
        .expect_err("a dropped connection should fail, not hang forever");
    assert_eq!(error.code, codes::INTERNAL_ERROR);
    assert!(error.message.contains("app-server"), "{}", error.message);
}
