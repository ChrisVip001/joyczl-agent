//! 待办清单的契约：三条硬边界、整表替换、版本号单调、以及「读」的那一半。

use std::sync::Arc;

use serde_json::json;

use super::todo::{todo_write, TodoBoard, MAX_CONTENT_CHARS, MAX_ITEMS};

/// 一个最小可用的工具环境（清单本身与 ctx 无关，但 handler 要 session_id）。
async fn ctx(session: &str) -> super::ToolCtx {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    let _ = dir.keep();
    super::ToolCtx {
        facts: joyczl_state::Facts::new(pool.clone()),
        episodes: joyczl_state::Episodes::new(pool.clone()),
        chat: joyczl_state::Chat::new(pool.clone()),
        calendar: joyczl_state::Calendar::new(pool),
        home,
        session_id: session.to_string(),
        approval: None,
        jobs: None,
        hooks: None,
    }
}

async fn call(board: &Arc<TodoBoard>, session: &str, args: serde_json::Value) -> String {
    let tool = todo_write(board.clone());
    match (tool.handler)(ctx(session).await, args).await {
        Ok(text) => text,
        Err(e) => panic!("不该失败：{e}"),
    }
}

/// 写进去再读回来：整表替换，版本号加一。
#[tokio::test]
async fn writing_replaces_the_whole_list_and_bumps_the_revision() {
    let board = Arc::new(TodoBoard::new());

    let first = call(
        &board,
        "s1",
        json!({"todos": [
            {"content": "写测试", "status": "in_progress"},
            {"content": "改实现", "status": "pending"}
        ]}),
    )
    .await;
    assert!(first.contains("第 1 版"), "{first}");
    assert!(first.contains("in_progress: 写测试"), "{first}");
    assert!(first.contains("pending: 改实现"), "{first}");

    // 第二次是**整表替换**：没列出来的项就不在了。
    let second = call(
        &board,
        "s1",
        json!({"todos": [{"content": "只剩这一项", "status": "completed"}]}),
    )
    .await;
    assert!(second.contains("第 2 版"), "{second}");
    assert!(second.contains("completed: 只剩这一项"), "{second}");
    assert!(!second.contains("写测试"), "没列出来的项该消失：{second}");
    assert!(second.contains("1 项"), "{second}");
}

/// 省略 `todos` = 只读回全量。
#[tokio::test]
async fn omitting_todos_reads_the_list_back() {
    let board = Arc::new(TodoBoard::new());

    let empty = call(&board, "s1", json!({})).await;
    assert!(empty.contains("还没有待办清单"), "{empty}");

    call(
        &board,
        "s1",
        json!({"todos": [{"content": "甲", "status": "pending"}]}),
    )
    .await;
    let read = call(&board, "s1", json!({})).await;
    assert!(read.contains("pending: 甲"), "{read}");
}

/// 清单按会话隔离：别人的表不是我的表。
#[tokio::test]
async fn lists_are_per_session() {
    let board = Arc::new(TodoBoard::new());
    call(
        &board,
        "a",
        json!({"todos": [{"content": "甲的活", "status": "pending"}]}),
    )
    .await;
    call(
        &board,
        "b",
        json!({"todos": [{"content": "乙的活", "status": "pending"}]}),
    )
    .await;

    let a = call(&board, "a", json!({})).await;
    assert!(a.contains("甲的活") && !a.contains("乙的活"), "{a}");
}

/// 三条硬边界：项数、内容、同时只能一个 in_progress。
#[tokio::test]
async fn the_three_hard_limits_are_enforced() {
    let board = Arc::new(TodoBoard::new());

    let too_many: Vec<_> = (0..MAX_ITEMS + 1)
        .map(|n| json!({"content": format!("第 {n} 项"), "status": "pending"}))
        .collect();
    let out = call(&board, "s", json!({"todos": too_many})).await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains(&format!("最多 {MAX_ITEMS} 项")), "{out}");

    let out = call(
        &board,
        "s",
        json!({"todos": [{"content": "   ", "status": "pending"}]}),
    )
    .await;
    assert!(out.contains("content 是空的"), "{out}");

    let long = "字".repeat(MAX_CONTENT_CHARS + 1);
    let out = call(
        &board,
        "s",
        json!({"todos": [{"content": long, "status": "pending"}]}),
    )
    .await;
    assert!(out.contains("超过"), "{out}");

    let out = call(
        &board,
        "s",
        json!({"todos": [
            {"content": "甲", "status": "in_progress"},
            {"content": "乙", "status": "in_progress"}
        ]}),
    )
    .await;
    assert!(out.contains("只能有一项 in_progress"), "{out}");
}

/// 校验不过时**一个字都不改**：半写进去的清单比没写更坏。
#[tokio::test]
async fn a_rejected_write_leaves_the_old_list_alone() {
    let board = Arc::new(TodoBoard::new());
    let good = call(
        &board,
        "s",
        json!({"todos": [{"content": "好的", "status": "pending"}]}),
    )
    .await;
    assert!(good.contains("第 1 版"), "{good}");

    let out = call(
        &board,
        "s",
        json!({"todos": [
            {"content": "甲", "status": "in_progress"},
            {"content": "乙", "status": "in_progress"}
        ]}),
    )
    .await;
    assert!(out.starts_with("Error:"), "{out}");

    let after = call(&board, "s", json!({})).await;
    assert!(after.contains("第 1 版"), "版本号不该动：{after}");
    assert!(after.contains("pending: 好的"), "{after}");
}

/// 不认识的 status 要指出是哪一项、收到了什么。
#[tokio::test]
async fn an_unknown_status_says_which_item_and_what_was_seen() {
    let board = Arc::new(TodoBoard::new());
    let out = call(
        &board,
        "s",
        json!({"todos": [{"content": "甲", "status": "done"}]}),
    )
    .await;
    assert!(out.contains("第 1 项"), "{out}");
    assert!(out.contains("done"), "{out}");
    assert!(out.contains("in_progress"), "要把认得的列出来：{out}");
}

/// 注入用的那一段：固定标题 + 一行一项（压缩之后也认得出）。
#[test]
fn the_injection_has_a_stable_header() {
    let board = TodoBoard::new();
    assert!(board.render("s").is_none(), "没清单就不加标题");

    board
        .write(
            "s",
            vec![super::todo::TodoItem {
                content: "写测试".to_string(),
                status: super::todo::TodoStatus::InProgress,
            }],
        )
        .expect("写进去");
    let rendered = board.render("s").expect("有清单了");
    assert!(
        rendered.starts_with(super::todo::INJECTION_HEADER),
        "{rendered}"
    );
    assert!(rendered.contains("in_progress: 写测试"), "{rendered}");
}
