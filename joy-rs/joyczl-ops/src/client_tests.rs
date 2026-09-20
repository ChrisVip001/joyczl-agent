//! 传输层的测试。假服务端就是内存里的两条管子 —— 不需要真的起进程，
//! 也不需要真的磁盘：这一层该被验证的只有「帧怎么进出」，那就只测这个。

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::io::{BufReader, DuplexStream, ReadHalf, WriteHalf};

use super::*;

/// 假服务端的另一头：读请求、写应答和通知。
struct Fake {
    requests: BufReader<ReadHalf<DuplexStream>>,
    to_client: WriteHalf<DuplexStream>,
}

/// 一条内存连接。左边是我们的客户端，右边是假的 app-server。
fn fake() -> (AppServer, Fake) {
    let (ours, theirs) = tokio::io::duplex(8192);
    let (reader, writer) = tokio::io::split(ours);
    let (theirs_read, theirs_write) = tokio::io::split(theirs);
    let app = AppServer::attach(None, reader, writer);
    let fake = Fake {
        requests: BufReader::new(theirs_read),
        to_client: theirs_write,
    };
    (app, fake)
}

impl Fake {
    /// 收一条请求（客户端发来的那一行）。
    async fn next_request(&mut self) -> Value {
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

    async fn respond(&mut self, id: &Value, result: Value) {
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

    /// 原样写一行，用来喂坏数据。
    async fn raw(&mut self, text: &str) {
        let line = format!("{text}\n");
        self.to_client
            .write_all(line.as_bytes())
            .await
            .expect("写给客户端");
    }
}

/// 在后台起一个请求，好让测试同时握着管子两头。
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

    // 倒着回：先回后到的那条。配错的话这里就会串味。
    fake.respond(&two["id"], json!({"data": ["second"]})).await;
    fake.respond(&one["id"], json!({"data": ["first"]})).await;

    assert_eq!(second.await.unwrap().unwrap()["data"][0], "second");
    assert_eq!(first.await.unwrap().unwrap()["data"][0], "first");
}

#[tokio::test]
async fn 通知原样广播出来() {
    let (app, mut fake) = fake();
    // 先订阅再发请求 —— 这是真代码里的规矩，测试也照做。
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
    // 原样传回：驾驶舱要把这句话直接显示给用户，换成「请求失败」就白写了。
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

    // 把管子那头扔掉：等于 app-server 退出了。
    drop(fake);

    let error = call
        .await
        .unwrap()
        .expect_err("连接断了就该报错，而不是永远挂着");
    assert_eq!(error.code, codes::INTERNAL_ERROR);
    assert!(error.message.contains("app-server"), "{}", error.message);
}
