//! Write-tool tests: gate semantics (roots, content checks, approval), edit matching,
//! and the change-hunk rendering contract. No network, real temp dirs.

use std::sync::{Arc, Mutex};

use serde_json::json;

use super::approval::{ApprovalBroker, ApprovalFut, ApprovalRequest, RememberTarget};
use super::fs_manage::{delete_file, move_file};
use super::fs_write::{edit, render_change, write_file, WritePolicy};
use crate::{ToolCtx, ToolRegistry};

async fn test_ctx(approval: Option<Arc<dyn ApprovalBroker>>) -> ToolCtx {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("open the store");
    let _ = dir.keep(); // sqlite still writes -wal/-shm: the dir must not be deleted here
    ToolCtx {
        on_uncertain: None,
        resource_lease: None,
        task_run_id: None,
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
        self.seen.lock().expect("lock").push(request);
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
            json!({"path": path.display().to_string(), "content": "first line\nsecond line\n"}),
        )
        .await;
    assert!(out.starts_with("Wrote"), "{out}");
    assert!(out.contains("(new, 2 lines)"), "{out}");
    assert_eq!(
        std::fs::read_to_string(&path).expect("the file should exist"),
        "first line\nsecond line\n"
    );
}

#[tokio::test]
async fn writing_identical_content_skips_the_disk() {
    let ctx = test_ctx(None).await;
    let reg = registry();
    let path = ctx.home.join("same.md");
    let args = |content: &str| json!({"path": path.display().to_string(), "content": content});
    let first = reg
        .execute(ctx.clone(), "write_file", args("the same content\n"))
        .await;
    assert!(first.starts_with("Wrote"), "{first}");
    let again = reg
        .execute(ctx.clone(), "write_file", args("the same content\n"))
        .await;
    assert!(again.contains("content is unchanged"), "{again}");
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
    assert!(out.contains("outside the writable scope"), "{out}");
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
    assert!(
        out.contains("No working directory has been chosen yet"),
        "{out}"
    );
    assert!(!path.exists());
}

/// The workspace is the *whole* boundary: the Joy home is not writable just because it is ours.
#[tokio::test]
async fn a_write_outside_the_chosen_workspace_is_refused_even_under_home() {
    let mut ctx = test_ctx(None).await;
    let workspace = ctx.home.join("workspace");
    std::fs::create_dir_all(&workspace).expect("create the working directory");
    ctx.cwd = Some(workspace);
    let path = ctx.home.join("memory/escape.md");
    let out = registry()
        .execute(
            ctx,
            "write_file",
            json!({"path": path.display().to_string(), "content": "hi\n"}),
        )
        .await;
    assert!(out.contains("outside the writable scope"), "{out}");
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
    assert!(out.contains("limit"), "{out}");
}

#[tokio::test]
async fn edit_replaces_the_single_match_and_shows_the_hunk() {
    let ctx = test_ctx(None).await;
    let reg = registry();
    let path = ctx.home.join("article.md");
    std::fs::write(&path, "alpha beta gamma\nsecond line\n").expect("place the file");
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
    assert!(out.starts_with("Edited"), "{out}");
    assert!(out.contains("- alpha beta gamma"), "{out}");
    assert!(out.contains("+ alpha delta gamma"), "{out}");
    assert_eq!(
        std::fs::read_to_string(&path).expect("it should be rewritten"),
        "alpha delta gamma\nsecond line\n"
    );
}

#[tokio::test]
async fn edit_refuses_zero_matches_with_a_hint() {
    let ctx = test_ctx(None).await;
    let reg = registry();
    let path = ctx.home.join("article.md");
    std::fs::write(&path, "alpha beta gamma\n").expect("place the file");
    let out = reg
        .execute(
            ctx,
            "edit",
            json!({"path": path.display().to_string(), "old_string": "zeta", "new_string": "x"}),
        )
        .await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("was not found"), "{out}");
    assert!(out.contains("read_file"), "{out}");
}

#[tokio::test]
async fn edit_refuses_ambiguous_matches() {
    let ctx = test_ctx(None).await;
    let reg = registry();
    let path = ctx.home.join("article.md");
    std::fs::write(&path, "beta here\nbeta there\n").expect("place the file");
    let out = reg
        .execute(
            ctx,
            "edit",
            json!({"path": path.display().to_string(), "old_string": "beta", "new_string": "x"}),
        )
        .await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("2 times"), "{out}");
    assert_eq!(
        std::fs::read_to_string(&path).expect("a refusal must not touch the file"),
        "beta here\nbeta there\n"
    );
}

#[tokio::test]
async fn edit_refuses_a_no_op_and_a_missing_file() {
    let ctx = test_ctx(None).await;
    let reg = registry();
    let path = ctx.home.join("article.md");
    std::fs::write(&path, "alpha beta\n").expect("place the file");
    let same = reg
        .execute(
            ctx.clone(),
            "edit",
            json!({"path": path.display().to_string(), "old_string": "beta", "new_string": "beta"}),
        )
        .await;
    assert!(same.starts_with("Error:"), "{same}");
    assert!(same.contains("the same"), "{same}");

    let missing_path = ctx.home.join("nope.md");
    let missing = reg
        .execute(
            ctx,
            "edit",
            json!({"path": missing_path.display().to_string(), "old_string": "a", "new_string": "b"}),
        )
        .await;
    assert!(missing.starts_with("Error:"), "{missing}");
    assert!(missing.contains("File not found"), "{missing}");
}

#[tokio::test]
async fn edit_refuses_a_binary_file() {
    let ctx = test_ctx(None).await;
    let reg = registry();
    let path = ctx.home.join("blob.bin");
    std::fs::write(&path, [0xff, 0xfe, 0x00]).expect("place the file");
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
    assert!(out.starts_with("Wrote"), "{out}");
    {
        let seen = yes.seen.lock().expect("lock");
        assert_eq!(seen.len(), 1, "asked once, not zero times and not many");
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
    assert!(out.contains("not approved"), "{out}");
    assert!(!denied_path.exists());
}

/// "remember this one" on a write names the **directory** (never the file name), and an approved directory
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
    assert!(out.starts_with("Wrote"), "{out}");
    {
        let seen = yes.seen.lock().expect("lock");
        assert_eq!(seen.len(), 1, "the first time has to ask");
        assert_eq!(
            seen[0].remember,
            Some(RememberTarget::Directory(ctx.home.clone())),
            "what is remembered is the directory, not the file name"
        );
    }

    // What "always allow in this session" does on the server side: the directory lands in the
    // shared cell.
    approved.lock().expect("lock").push(ctx.home.clone());
    let second = ctx.home.join("second.md");
    let out = reg
        .execute(
            ctx.clone(),
            "write_file",
            json!({"path": second.display().to_string(), "content": "2\n"}),
        )
        .await;
    assert!(out.starts_with("Wrote"), "{out}");
    assert_eq!(
        yes.seen.lock().expect("lock").len(),
        1,
        "an approved directory must not be asked about twice"
    );
}

/// An approved directory outside the writable boundary unlocks nothing: containment runs first.
#[tokio::test]
async fn an_approved_directory_cannot_widen_the_boundary() {
    let ctx = test_ctx(Some(Fake::answering(true))).await;
    let outside = tempfile::tempdir().expect("another directory");
    let mut reg = ToolRegistry::new();
    let approved: Arc<Mutex<Vec<std::path::PathBuf>>> = Default::default();
    approved
        .lock()
        .expect("lock")
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
    assert!(out.contains("writable scope"), "{out}");
    assert!(
        !path.exists(),
        "a write outside the workspace must not reach the disk"
    );
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
    assert!(out.contains("nobody can answer"), "{out}");
}

#[test]
fn render_change_pins_the_hunk_format() {
    // New file.
    assert_eq!(render_change(None, "a\nb\n"), "(new, 2 lines)");

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
    assert_eq!(
        render_change(Some("same\n"), "same\n"),
        "(content unchanged)"
    );
}

#[test]
fn render_change_truncates_honestly() {
    let old = "x\n".repeat(100);
    let new = "y\n".repeat(100);
    let hunk = render_change(Some(&old), &new);
    assert!(hunk.contains("diff truncated"), "{hunk}");
    assert!(hunk.contains("read_file"), "{hunk}");
}

// ---- delete_file / move_file ---------------------------------------------------------------

async fn run_tool(
    reg: &ToolRegistry,
    ctx: &ToolCtx,
    name: &str,
    args: serde_json::Value,
) -> String {
    reg.execute(ctx.clone(), name, args).await
}

fn delete_move_registry(policy: WritePolicy) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(delete_file(policy.clone()));
    reg.register(move_file(policy));
    reg
}

#[tokio::test]
async fn delete_file_removes_a_file_and_says_how_big_it_was() {
    let ctx = test_ctx(None).await;
    let target = ctx.home.join("scratch.txt");
    std::fs::write(&target, "one\ntwo\n").expect("write");

    let out = run_tool(
        &delete_move_registry(WritePolicy::default()),
        &ctx,
        "delete_file",
        json!({"path": "scratch.txt"}),
    )
    .await;
    assert!(out.starts_with("Deleted"), "{out}");
    assert!(
        out.contains("8 bytes"),
        "the size is part of the receipt: {out}"
    );
    assert!(!target.exists(), "the file has to be gone");
}

#[tokio::test]
async fn delete_file_refuses_a_directory_and_a_missing_file() {
    let ctx = test_ctx(None).await;
    std::fs::create_dir_all(ctx.home.join("tree/inner")).expect("mkdir");

    let out = run_tool(
        &delete_move_registry(WritePolicy::default()),
        &ctx,
        "delete_file",
        json!({"path": "tree"}),
    )
    .await;
    assert!(out.contains("is a directory"), "{out}");
    assert!(
        ctx.home.join("tree/inner").is_dir(),
        "nothing may be removed"
    );

    let out = run_tool(
        &delete_move_registry(WritePolicy::default()),
        &ctx,
        "delete_file",
        json!({"path": "nope.txt"}),
    )
    .await;
    assert!(out.contains("nothing to delete"), "{out}");
}

/// The boundary comes first: an approved-but-outside path is still refused — the same rule
/// `write_file` follows, or a delete would be the way around it.
#[tokio::test]
async fn delete_file_stays_inside_the_workspace() {
    let ctx = test_ctx(None).await;
    let outside = tempfile::tempdir().expect("temp dir");
    let victim = outside.path().join("keep.txt");
    std::fs::write(&victim, "keep me\n").expect("write");

    let out = run_tool(
        &delete_move_registry(WritePolicy::default()),
        &ctx,
        "delete_file",
        json!({"path": victim.to_string_lossy()}),
    )
    .await;
    assert!(out.starts_with("Error"), "{out}");
    assert!(victim.exists(), "a path outside the roots is never deleted");
}

#[tokio::test]
async fn move_file_renames_and_creates_missing_parents() {
    let ctx = test_ctx(None).await;
    std::fs::write(ctx.home.join("a.txt"), "content\n").expect("write");

    let out = run_tool(
        &delete_move_registry(WritePolicy::default()),
        &ctx,
        "move_file",
        json!({"from": "a.txt", "to": "deep/dir/b.txt"}),
    )
    .await;
    assert!(out.starts_with("Moved"), "{out}");
    assert!(!ctx.home.join("a.txt").exists(), "the source is gone");
    assert_eq!(
        std::fs::read_to_string(ctx.home.join("deep/dir/b.txt")).expect("read"),
        "content\n"
    );
}

/// Overwriting is refused on purpose: `mv`-style silent replacement of a file the model never
/// read is a success nobody notices until the content is gone.
#[tokio::test]
async fn move_file_refuses_to_overwrite() {
    let ctx = test_ctx(None).await;
    std::fs::write(ctx.home.join("a.txt"), "new\n").expect("write");
    std::fs::write(ctx.home.join("b.txt"), "old\n").expect("write");

    let out = run_tool(
        &delete_move_registry(WritePolicy::default()),
        &ctx,
        "move_file",
        json!({"from": "a.txt", "to": "b.txt"}),
    )
    .await;
    assert!(out.contains("already exists"), "{out}");
    assert_eq!(
        std::fs::read_to_string(ctx.home.join("b.txt")).expect("read"),
        "old\n",
        "the destination must not change"
    );
    assert!(ctx.home.join("a.txt").exists());
}

/// A move is a delete plus a write, so **both** ends need to be inside the boundary — an
/// approved destination must not be a way to pull a file out of somewhere else.
#[tokio::test]
async fn move_file_gates_both_ends() {
    let ctx = test_ctx(None).await;
    let outside = tempfile::tempdir().expect("temp dir");
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, "classified\n").expect("write");

    let out = run_tool(
        &delete_move_registry(WritePolicy::default()),
        &ctx,
        "move_file",
        json!({"from": secret.to_string_lossy(), "to": "stolen.txt"}),
    )
    .await;
    assert!(out.starts_with("Error"), "{out}");
    assert!(secret.exists(), "the source must stay where it is");
    assert!(!ctx.home.join("stolen.txt").exists());

    let out = run_tool(
        &delete_move_registry(WritePolicy::default()),
        &ctx,
        "move_file",
        json!({"from": "nothing.txt", "to": "elsewhere.txt"}),
    )
    .await;
    assert!(out.contains("nothing to move"), "{out}");
}
