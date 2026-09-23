//! 待办清单在**一轮里的位置**：它被注入 system prompt，所以压缩、滑窗都冲不掉。

use std::sync::Arc;

use joyczl_config::Settings;
use joyczl_protocol::{RequestId, TurnStartParams};
use joyczl_provider::mock::Mock;
use joyczl_provider::Resolved;
use serde_json::json;

use crate::{EventSink, Server};

/// 同一个 server 跑两轮：清单是**会话级**的，必须落在 server 上而不是某一轮里。
async fn run_twice(first_message: &str, second_message: &str) -> (Arc<Mock>, String, String) {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    let _ = dir.keep();

    let mock = Arc::new(Mock::new(vec![
        // 第一轮：检索门 → 写清单 → 收尾
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "todo"}"#),
        Mock::tool_use(
            "call-1",
            "todo_write",
            json!({"todos": [
                {"content": "写测试", "status": "in_progress"},
                {"content": "改文档", "status": "pending"}
            ]}),
        ),
        Mock::text("清单记下了。"),
        // 第二轮：检索门 → 收尾
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "todo"}"#),
        Mock::text("接着干。"),
    ]));

    let server = Server::boot(
        pool,
        Settings {
            home: home.clone(),
            ..Settings::default()
        },
    )
    .await;
    server.set_resolved(Some(Resolved {
        provider_id: "mock".to_string(),
        client: mock.clone(),
        model: "test-model".to_string(),
        small_model: "test-small".to_string(),
    }));

    let mut tools_first = String::new();
    let mut system_second = String::new();
    for (index, message) in [first_message, second_message].into_iter().enumerate() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = EventSink::new(tx);
        crate::turn::run_turn(
            &server,
            TurnStartParams {
                session_id: Some("todo-session".to_string()),
                message: message.to_string(),
                stream: Some(true),
            },
            RequestId::Number(index as i32 + 1),
            &sink,
        )
        .await
        .expect("这一轮该跑完");

        while let Ok(frame) = rx.try_recv() {
            if let crate::Frame::Notification(joyczl_protocol::ServerNotification::ToolCompleted(
                done,
            )) = frame
            {
                if index == 0 {
                    tools_first = done.output.clone();
                }
            }
        }
    }

    // 最后一次请求就是第二轮的循环调用（前面那些是检索门之类的短请求）——
    // 不看固定下标，免得「门算不算一次请求」一变，测试就假红。
    let requests = mock.received.lock().unwrap();
    if let Some(last) = requests.last() {
        system_second = last.system.clone().unwrap_or_default();
    }
    drop(requests);

    (mock, tools_first, system_second)
}

/// 清单写完之后**下一轮就看得到**，而且是注入的（不靠历史）。
#[tokio::test]
async fn a_todo_list_is_injected_into_the_next_turn() {
    let (_mock, tool_output, system) = run_twice("列一下要做的事", "接着来").await;

    assert!(
        tool_output.contains("in_progress: 写测试"),
        "工具应答里该有权威清单：{tool_output}"
    );
    assert!(
        system.contains(joyczl_tools::todo::INJECTION_HEADER),
        "第二轮的 system prompt 里该有清单那一段：{system}"
    );
    assert!(system.contains("in_progress: 写测试"), "{system}");
    assert!(system.contains("pending: 改文档"), "{system}");
}

/// 没写过清单的会话不该多出那一段（空标题比没有更糟：它会让人以为清单是空的）。
#[tokio::test]
async fn a_session_without_a_list_gets_no_section() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    let _ = dir.keep();
    let mock = Arc::new(Mock::new(vec![
        Mock::text(r#"{"retrieve": false, "query": "", "reason": "plain"}"#),
        Mock::text("好。"),
    ]));
    let server = Server::boot(
        pool,
        Settings {
            home: home.clone(),
            ..Settings::default()
        },
    )
    .await;
    server.set_resolved(Some(Resolved {
        provider_id: "mock".to_string(),
        client: mock.clone(),
        model: "test-model".to_string(),
        small_model: "test-small".to_string(),
    }));

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    crate::turn::run_turn(
        &server,
        TurnStartParams {
            session_id: Some("clean".to_string()),
            message: "随便说点什么".to_string(),
            stream: Some(true),
        },
        RequestId::Number(1),
        &EventSink::new(tx),
    )
    .await
    .expect("跑完");

    let requests = mock.received.lock().unwrap();
    let system = requests[0].system.clone().unwrap_or_default();
    assert!(
        !system.contains(joyczl_tools::todo::INJECTION_HEADER),
        "没清单就不该有那一段：{system}"
    );
}
