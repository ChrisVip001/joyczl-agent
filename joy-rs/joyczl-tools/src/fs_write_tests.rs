//! Write-tool tests: gate semantics (roots, content checks, approval), edit matching,
//! and the change-hunk rendering contract. No network, real temp dirs.

use std::sync::{Arc, Mutex};

use serde_json::json;

use super::approval::{ApprovalBroker, ApprovalFut, ApprovalRequest, RememberTarget};
use super::fs_write::{edit, render_change, write_file, WritePolicy};
use crate::{ToolCtx, ToolRegistry};

async fn test_ctx(approval: Option<Arc<dyn ApprovalBroker>>) -> ToolCtx {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    let _ = dir.keep(); // sqlite still writes -wal/-shm: the dir must not be deleted here
    ToolCtx {
        activate: None,
        embedder: None,
        // The suite writes under `home`; treat it as the human's chosen workspace.
        cwd: Some(home.clone()),
        approval,
        jobs: None,
        hooks: None,
        turn_limits: std::sync::Arc::new(crate::web::TurnLimits::default()),
        session_id: "test".to_string(),
        facts: joyczl_state::Facts::new(pool.clone()),
        episodes: joyczl_state::Episodes::new(pool.clone()),
        chat: joyczl_state::Chat::new(pool.clone()),
        calendar: joyczl_state::Calendar::new(pool),
        home,
        on_delta: None,
    }
}

fn registry() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    let policy = WritePolicy::default();
    reg.register(write_file(policy.clone()));
    reg.register(edit(policy));
    reg
}

/// A broker that records questions and answers by script (same shape as approval_tests).
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

#[tokio::test]
async fn write_file_creates_a_file_and_reports_it() {
    let ctx = test_ctx(None).await;
    let reg = registry();
    let path = ctx.home.join("notes/created.md");
    let out = reg
        .execute(
            ctx.clone(),
            "write_file",
            json!({"path": path.display().to_string(), "content": "第一行\n第二行\n"}),
        )
        .await;
    assert!(out.starts_with("已写入"), "{out}");
    assert!(out.contains("（新建，2 行）"), "{out}");
    assert_eq!(
        std::fs::read_to_string(&path).expect("文件应存在"),
        "第一行\n第二行\n"
    );
}

#[tokio::test]
async fn writing_identical_content_skips_the_disk() {
    let ctx = test_ctx(None).await;
    let reg = registry();
    let path = ctx.home.join("same.md");
    let args = |content: &str| json!({"path": path.display().to_string(), "content": content});
    let first = reg
        .execute(ctx.clone(), "write_file", args("同一段内容\n"))
        .await;
    assert!(first.starts_with("已写入"), "{first}");
    let again = reg
        .execute(ctx.clone(), "write_file", args("同一段内容\n"))
        .await;
    assert!(again.contains("内容未变，没有写盘"), "{again}");
}

#[tokio::test]
async fn a_write_outside_the_roots_is_refused_without_asking() {
    let ctx = test_ctx(None).await;
    let reg = registry();
    let out = reg
        .execute(
            ctx,
            "write_file",
            json!({"path": "/etc/joy-fs-write-test-denied.txt", "content": "hi\n"}),
        )
        .await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("可写范围之外"), "{out}");
    assert!(!std::path::Path::new("/etc/joy-fs-write-test-denied.txt").exists());
}

/// Choosing a workspace is the human's capability switch: with none selected, even a path
/// inside the Joy home is refused — and the refusal says what to do instead of just "outside".
#[tokio::test]
async fn no_chosen_workspace_means_no_writes_at_all() {
    let mut ctx = test_ctx(None).await;
    ctx.cwd = None;
    let reg = registry();
    let path = ctx.home.join("notes/nope.md");
    let out = reg
        .execute(
            ctx,
            "write_file",
            json!({"path": path.display().to_string(), "content": "hi\n"}),
        )
        .await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("还没有选择工作目录"), "{out}");
    assert!(!path.exists());
}

/// The workspace is the *whole* boundary: the Joy home is not writable just because it is ours.
#[tokio::test]
async fn a_write_outside_the_chosen_workspace_is_refused_even_under_home() {
    let mut ctx = test_ctx(None).await;
    let workspace = ctx.home.join("workspace");
    std::fs::create_dir_all(&workspace).expect("建工作目录");
    ctx.cwd = Some(workspace);
    let path = ctx.home.join("memory/escape.md");
    let out = registry()
        .execute(
            ctx,
            "write_file",
            json!({"path": path.display().to_string(), "content": "hi\n"}),
        )
        .await;
    assert!(out.contains("可写范围之外"), "{out}");
    assert!(!path.exists());
}

#[tokio::test]
async fn binary_content_is_refused() {
    let ctx = test_ctx(None).await;
    let reg = registry();
    let path = ctx.home.join("bin.md");
    let out = reg
        .execute(
            ctx,
            "write_file",
            json!({"path": path.display().to_string(), "content": "a\0b"}),
        )
        .await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("NUL"), "{out}");
}

#[tokio::test]
async fn oversized_content_is_refused() {
    let ctx = test_ctx(None).await;
    let reg = registry();
    let big = "x".repeat(super::fs_write::MAX_WRITE_BYTES + 1);
    let path = ctx.home.join("big.txt");
    let out = reg
        .execute(
            ctx,
            "write_file",
            json!({"path": path.display().to_string(), "content": big}),
        )
        .await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("上限"), "{out}");
}

#[tokio::test]
async fn edit_replaces_the_single_match_and_shows_the_hunk() {
    let ctx = test_ctx(None).await;
    let reg = registry();
    let path = ctx.home.join("article.md");
    std::fs::write(&path, "alpha beta gamma\nsecond line\n").expect("装置文件");
    let out = reg
        .execute(
            ctx,
            "edit",
            json!({
                "path": path.display().to_string(),
                "old_string": "beta",
                "new_string": "delta"
            }),
        )
        .await;
    assert!(out.starts_with("已编辑"), "{out}");
    assert!(out.contains("- alpha beta gamma"), "{out}");
    assert!(out.contains("+ alpha delta gamma"), "{out}");
    assert_eq!(
        std::fs::read_to_string(&path).expect("应已改写"),
        "alpha delta gamma\nsecond line\n"
    );
}

#[tokio::test]
async fn edit_refuses_zero_matches_with_a_hint() {
    let ctx = test_ctx(None).await;
    let reg = registry();
    let path = ctx.home.join("article.md");
    std::fs::write(&path, "alpha beta gamma\n").expect("装置文件");
    let out = reg
        .execute(
            ctx,
            "edit",
            json!({"path": path.display().to_string(), "old_string": "zeta", "new_string": "x"}),
        )
        .await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("没有找到"), "{out}");
    assert!(out.contains("read_file"), "{out}");
}

#[tokio::test]
async fn edit_refuses_ambiguous_matches() {
    let ctx = test_ctx(None).await;
    let reg = registry();
    let path = ctx.home.join("article.md");
    std::fs::write(&path, "beta here\nbeta there\n").expect("装置文件");
    let out = reg
        .execute(
            ctx,
            "edit",
            json!({"path": path.display().to_string(), "old_string": "beta", "new_string": "x"}),
        )
        .await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("2 次"), "{out}");
    assert_eq!(
        std::fs::read_to_string(&path).expect("被拒绝时文件不能动"),
        "beta here\nbeta there\n"
    );
}

#[tokio::test]
async fn edit_refuses_a_no_op_and_a_missing_file() {
    let ctx = test_ctx(None).await;
    let reg = registry();
    let path = ctx.home.join("article.md");
    std::fs::write(&path, "alpha beta\n").expect("装置文件");
    let same = reg
        .execute(
            ctx.clone(),
            "edit",
            json!({"path": path.display().to_string(), "old_string": "beta", "new_string": "beta"}),
        )
        .await;
    assert!(same.starts_with("Error:"), "{same}");
    assert!(same.contains("相同"), "{same}");

    let missing_path = ctx.home.join("nope.md");
    let missing = reg
        .execute(
            ctx,
            "edit",
            json!({"path": missing_path.display().to_string(), "old_string": "a", "new_string": "b"}),
        )
        .await;
    assert!(missing.starts_with("Error:"), "{missing}");
    assert!(missing.contains("找不到文件"), "{missing}");
}

#[tokio::test]
async fn edit_refuses_a_binary_file() {
    let ctx = test_ctx(None).await;
    let reg = registry();
    let path = ctx.home.join("blob.bin");
    std::fs::write(&path, [0xff, 0xfe, 0x00]).expect("装置文件");
    let out = reg
        .execute(
            ctx,
            "edit",
            json!({"path": path.display().to_string(), "old_string": "a", "new_string": "b"}),
        )
        .await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("UTF-8"), "{out}");
}

#[tokio::test]
async fn on_request_mode_asks_once_per_write() {
    let yes = Fake::answering(true);
    let ctx = test_ctx(Some(yes.clone())).await;
    let mut reg = ToolRegistry::new();
    let policy = WritePolicy {
        approval: true,
        approval_timeout_secs: 5,
        extra_roots: Vec::new(),
        approved_dirs: Default::default(),
    };
    reg.register(write_file(policy.clone()));
    reg.register(edit(policy));

    let path = ctx.home.join("approved.md");
    let out = reg
        .execute(
            ctx.clone(),
            "write_file",
            json!({"path": path.display().to_string(), "content": "hi\n"}),
        )
        .await;
    assert!(out.starts_with("已写入"), "{out}");
    {
        let seen = yes.seen.lock().expect("锁");
        assert_eq!(seen.len(), 1, "问一次，不是零次也不是多次");
        assert_eq!(seen[0].tool, "write_file");
    } // guard dropped here — it must not live across the awaits below

    // A denial (or silence) skips the write — and the file must not appear.
    let no = Fake::answering(false);
    let ctx2 = test_ctx(Some(no)).await;
    let denied_path = ctx2.home.join("denied.md");
    let out = reg
        .execute(
            ctx2.clone(),
            "write_file",
            json!({"path": denied_path.display().to_string(), "content": "hi\n"}),
        )
        .await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("没有被批准"), "{out}");
    assert!(!denied_path.exists());
}

/// "记住这条" on a write names the **directory** (never the file name), and an approved directory
/// stops being asked about — the cell is shared, so the very next write sees it.
#[tokio::test]
async fn an_approved_directory_stops_asking() {
    let yes = Fake::answering(true);
    let ctx = test_ctx(Some(yes.clone())).await;
    let approved: Arc<Mutex<Vec<std::path::PathBuf>>> = Default::default();
    let mut reg = ToolRegistry::new();
    reg.register(write_file(WritePolicy {
        approval: true,
        approval_timeout_secs: 5,
        extra_roots: Vec::new(),
        approved_dirs: approved.clone(),
    }));

    let first = ctx.home.join("first.md");
    let out = reg
        .execute(
            ctx.clone(),
            "write_file",
            json!({"path": first.display().to_string(), "content": "1\n"}),
        )
        .await;
    assert!(out.starts_with("已写入"), "{out}");
    {
        let seen = yes.seen.lock().expect("锁");
        assert_eq!(seen.len(), 1, "第一次要问");
        assert_eq!(
            seen[0].remember,
            Some(RememberTarget::Directory(ctx.home.clone())),
            "记住的是目录，不是文件名"
        );
    }

    // What "本会话总是允许" does on the server side: the directory lands in the shared cell.
    approved.lock().expect("锁").push(ctx.home.clone());
    let second = ctx.home.join("second.md");
    let out = reg
        .execute(
            ctx.clone(),
            "write_file",
            json!({"path": second.display().to_string(), "content": "2\n"}),
        )
        .await;
    assert!(out.starts_with("已写入"), "{out}");
    assert_eq!(
        yes.seen.lock().expect("锁").len(),
        1,
        "已批准的目录不该再问第二次"
    );
}

/// An approved directory outside the writable boundary unlocks nothing: containment runs first.
#[tokio::test]
async fn an_approved_directory_cannot_widen_the_boundary() {
    let ctx = test_ctx(Some(Fake::answering(true))).await;
    let outside = tempfile::tempdir().expect("另一个目录");
    let mut reg = ToolRegistry::new();
    let approved: Arc<Mutex<Vec<std::path::PathBuf>>> = Default::default();
    approved
        .lock()
        .expect("锁")
        .push(outside.path().to_path_buf());
    reg.register(write_file(WritePolicy {
        approval: true,
        approval_timeout_secs: 5,
        extra_roots: Vec::new(),
        approved_dirs: approved,
    }));

    let path = outside.path().join("leak.md");
    let out = reg
        .execute(
            ctx,
            "write_file",
            json!({"path": path.display().to_string(), "content": "no\n"}),
        )
        .await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("可写范围"), "{out}");
    assert!(!path.exists(), "越界的写入不该落盘");
}

#[tokio::test]
async fn on_request_mode_without_a_broker_refuses() {
    let ctx = test_ctx(None).await; // approval mode on, nobody home to answer
    let mut reg = ToolRegistry::new();
    reg.register(write_file(WritePolicy {
        approval: true,
        approval_timeout_secs: 5,
        extra_roots: Vec::new(),
        approved_dirs: Default::default(),
    }));
    let path = ctx.home.join("x.md");
    let out = reg
        .execute(
            ctx,
            "write_file",
            json!({"path": path.display().to_string(), "content": "hi\n"}),
        )
        .await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("没人能回答"), "{out}");
}

#[test]
fn render_change_pins_the_hunk_format() {
    // New file.
    assert_eq!(render_change(None, "a\nb\n"), "（新建，2 行）");

    // In-place edit keeps shared head/tail and marks the middle.
    let hunk = render_change(
        Some("one\ntwo\nthree\nfour\nfive\n"),
        "one\ntwo\nTWO\nfour\nfive\n",
    );
    assert!(hunk.starts_with("@@"), "{hunk}");
    assert!(hunk.contains("- three"), "{hunk}");
    assert!(hunk.contains("+ TWO"), "{hunk}");
    assert!(hunk.contains("  one"), "{hunk}");

    // Identical content says so.
    assert_eq!(render_change(Some("same\n"), "same\n"), "（内容未变）");
}

#[test]
fn render_change_truncates_honestly() {
    let old = "x\n".repeat(100);
    let new = "y\n".repeat(100);
    let hunk = render_change(Some(&old), &new);
    assert!(hunk.contains("diff 截断"), "{hunk}");
    assert!(hunk.contains("read_file"), "{hunk}");
}
