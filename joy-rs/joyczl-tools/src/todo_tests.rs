//! Todo list contracts: three hard boundaries, whole-table replace, monotonic revision, and the "read" half.

use std::sync::Arc;

use serde_json::json;

use super::todo::{todo_write, TodoBoard, MAX_CONTENT_CHARS, MAX_ITEMS};

/// A minimal working tool environment (the list itself needs no ctx, but the handler wants session_id).
async fn ctx(session: &str) -> super::ToolCtx {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("open the store");
    let _ = dir.keep();
    super::ToolCtx {
        on_uncertain: None,
        resource_lease: None,
        task_run_id: None,
        activate: None,
        embedder: None,
        facts: joyczl_state::Facts::new(pool.clone()),
        episodes: joyczl_state::Episodes::new(pool.clone()),
        chat: joyczl_state::Chat::new(pool.clone()),
        calendar: joyczl_state::Calendar::new(pool),
        home,
        cwd: None,
        session_id: session.to_string(),
        approval: None,
        jobs: None,
        turn_limits: std::sync::Arc::new(crate::web::TurnLimits::default()),
        hooks: None,
        on_delta: None,
    }
}

async fn call(board: &Arc<TodoBoard>, session: &str, args: serde_json::Value) -> String {
    let tool = todo_write(board.clone());
    match (tool.handler)(ctx(session).await, args).await {
        Ok(text) => text,
        Err(e) => panic!("must not fail: {e}"),
    }
}

/// Write then read back: whole-table replace, revision +1.
#[tokio::test]
async fn writing_replaces_the_whole_list_and_bumps_the_revision() {
    let board = Arc::new(TodoBoard::new());

    let first = call(
        &board,
        "s1",
        json!({"todos": [
            {"content": "write tests", "status": "in_progress"},
            {"content": "change the implementation", "status": "pending"}
        ]}),
    )
    .await;
    assert!(first.contains("revision 1"), "{first}");
    assert!(first.contains("in_progress: write tests"), "{first}");
    assert!(
        first.contains("pending: change the implementation"),
        "{first}"
    );

    // The second write is a **whole-table replace**: items not listed are gone.
    let second = call(
        &board,
        "s1",
        json!({"todos": [{"content": "only this one is left", "status": "completed"}]}),
    )
    .await;
    assert!(second.contains("revision 2"), "{second}");
    assert!(
        second.contains("completed: only this one is left"),
        "{second}"
    );
    assert!(
        !second.contains("write tests"),
        "what was not listed should disappear: {second}"
    );
    assert!(second.contains("1 items"), "{second}");
}

/// Omitting `todos` = read the full list back only.
#[tokio::test]
async fn omitting_todos_reads_the_list_back() {
    let board = Arc::new(TodoBoard::new());

    let empty = call(&board, "s1", json!({})).await;
    assert!(empty.contains("There is no to-do list yet"), "{empty}");

    call(
        &board,
        "s1",
        json!({"todos": [{"content": "alpha", "status": "pending"}]}),
    )
    .await;
    let read = call(&board, "s1", json!({})).await;
    assert!(read.contains("pending: alpha"), "{read}");
}

/// Lists are isolated per session: someone else's table isn't mine.
#[tokio::test]
async fn lists_are_per_session() {
    let board = Arc::new(TodoBoard::new());
    call(
        &board,
        "a",
        json!({"todos": [{"content": "alpha's job", "status": "pending"}]}),
    )
    .await;
    call(
        &board,
        "b",
        json!({"todos": [{"content": "beta's job", "status": "pending"}]}),
    )
    .await;

    let a = call(&board, "a", json!({})).await;
    assert!(
        a.contains("alpha's job") && !a.contains("beta's job"),
        "{a}"
    );
}

/// Three hard boundaries: item count, content, and only one in_progress at a time.
#[tokio::test]
async fn the_three_hard_limits_are_enforced() {
    let board = Arc::new(TodoBoard::new());

    let too_many: Vec<_> = (0..MAX_ITEMS + 1)
        .map(|n| json!({"content": format!("item {n}"), "status": "pending"}))
        .collect();
    let out = call(&board, "s", json!({"todos": too_many})).await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains(&format!("at most {MAX_ITEMS} items")), "{out}");

    let out = call(
        &board,
        "s",
        json!({"todos": [{"content": "   ", "status": "pending"}]}),
    )
    .await;
    assert!(out.contains("has empty content"), "{out}");

    let long = "char".repeat(MAX_CONTENT_CHARS + 1);
    let out = call(
        &board,
        "s",
        json!({"todos": [{"content": long, "status": "pending"}]}),
    )
    .await;
    assert!(out.contains("over"), "{out}");

    let out = call(
        &board,
        "s",
        json!({"todos": [
            {"content": "alpha", "status": "in_progress"},
            {"content": "beta", "status": "in_progress"}
        ]}),
    )
    .await;
    assert!(out.contains("Only one item may be in_progress"), "{out}");
}

/// When validation fails, **nothing changes**: a half-written list is worse than none.
#[tokio::test]
async fn a_rejected_write_leaves_the_old_list_alone() {
    let board = Arc::new(TodoBoard::new());
    let good = call(
        &board,
        "s",
        json!({"todos": [{"content": "fine", "status": "pending"}]}),
    )
    .await;
    assert!(good.contains("revision 1"), "{good}");

    let out = call(
        &board,
        "s",
        json!({"todos": [
            {"content": "alpha", "status": "in_progress"},
            {"content": "beta", "status": "in_progress"}
        ]}),
    )
    .await;
    assert!(out.starts_with("Error:"), "{out}");

    let after = call(&board, "s", json!({})).await;
    assert!(
        after.contains("revision 1"),
        "the revision number must not move: {after}"
    );
    assert!(after.contains("pending: fine"), "{after}");
}

/// An unknown status must name the item and what was received.
#[tokio::test]
async fn an_unknown_status_says_which_item_and_what_was_seen() {
    let board = Arc::new(TodoBoard::new());
    let out = call(
        &board,
        "s",
        json!({"todos": [{"content": "alpha", "status": "done"}]}),
    )
    .await;
    assert!(out.contains("item 1"), "{out}");
    assert!(out.contains("done"), "{out}");
    assert!(
        out.contains("in_progress"),
        "the known ones have to be listed: {out}"
    );
}

/// The injected block: fixed header + one line per item (recognizable even after compaction).
#[test]
fn the_injection_has_a_stable_header() {
    let board = TodoBoard::new();
    assert!(board.render("s").is_none(), "no list, no heading");

    board
        .write(
            "s",
            vec![super::todo::TodoItem {
                content: "write tests".to_string(),
                status: super::todo::TodoStatus::InProgress,
            }],
        )
        .expect("write it in");
    let rendered = board.render("s").expect("there is a to-do list");
    assert!(
        rendered.starts_with(super::todo::INJECTION_HEADER),
        "{rendered}"
    );
    assert!(rendered.contains("in_progress: write tests"), "{rendered}");
}
