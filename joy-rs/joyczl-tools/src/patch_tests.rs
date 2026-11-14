//! `apply_patch`: what it refuses matters more than what it accepts — a half-applied patch gets
//! treated as "done" by whoever reads the diff next.

use serde_json::json;

use super::fs_write::WritePolicy;
use super::patch::{apply, apply_patch, parse, Op, PatchLine};
use crate::{ToolCtx, ToolRegistry};

async fn test_ctx() -> ToolCtx {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("open the store");
    let _ = dir.keep(); // sqlite also writes -wal/-shm: this directory must not be deleted in place
    ToolCtx {
        on_uncertain: None,
        resource_lease: None,
        task_run_id: None,
        activate: None,
        embedder: None,
        cwd: Some(home.clone()),
        approval: None,
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

const PATCH: &str = "*** Begin Patch
*** Update File: a.rs
@@ fn main @@
 fn main() {
-    old();
+    new();
 }
*** Add File: b.rs
+// a new file
*** End Patch
";

#[test]
fn it_parses_updates_and_adds_in_order() {
    let ops = parse(PATCH).expect("it should parse");
    assert_eq!(ops.len(), 2, "{ops:?}");
    match &ops[0] {
        Op::Update { path, hunks } => {
            assert_eq!(path, "a.rs");
            assert_eq!(hunks.len(), 1);
            assert_eq!(hunks[0].hint, "fn main");
            assert_eq!(
                hunks[0].lines,
                vec![
                    PatchLine::Context("fn main() {".to_string()),
                    PatchLine::Remove("    old();".to_string()),
                    PatchLine::Add("    new();".to_string()),
                    PatchLine::Context("}".to_string()),
                ]
            );
        }
        other => panic!("the first should be Update, actually {other:?}"),
    }
    match &ops[1] {
        Op::Add { path, lines } => {
            assert_eq!(path, "b.rs");
            assert_eq!(lines, &vec!["// a new file".to_string()]);
        }
        other => panic!("the second should be Add, actually {other:?}"),
    }
}

#[test]
fn prose_around_the_patch_is_tolerated() {
    let text = format!("here is the patch:\n```diff\n{PATCH}```\nplease apply it.");
    assert_eq!(
        parse(&text).expect("the wrapper should be ignored").len(),
        2
    );
}

#[test]
fn a_missing_end_marker_is_refused() {
    let why = parse("*** Begin Patch\n*** Add File: a\n+x\n")
        .expect_err("a half patch must not be accepted");
    assert!(why.contains("End Patch"), "{why}");
}

#[test]
fn an_unknown_directive_is_refused_not_skipped() {
    // Move is not implemented: ignoring it silently would leave "everything changed except the
    // move".
    let why =
        parse("*** Begin Patch\n*** Move to: b.rs\n*** End Patch\n").expect_err("it should refuse");
    assert!(why.contains("Move"), "{why}");
}

#[test]
fn an_add_file_without_plus_prefix_is_refused() {
    let why = parse("*** Begin Patch\n*** Add File: a\na bare line\n*** End Patch\n")
        .expect_err("it should refuse");
    assert!(why.contains('+'), "{why}");
}

#[test]
fn two_hunks_apply_at_two_different_places() {
    // The same two lines appear twice: once the cursor advances, the second hunk lands on the
    // second occurrence rather than editing the first again.
    let file = "same line\nkeep\nsame line\nkeep\n";
    let patch = "*** Begin Patch
*** Update File: a
@@ one @@
 same line
-keep
+edited one
@@ two @@
 same line
-keep
+edited
*** End Patch
";
    let ops = parse(patch).expect("parse");
    let Op::Update { hunks, .. } = &ops[0] else {
        panic!("should be Update");
    };
    let out = apply(file, hunks).expect("both places line up");
    assert_eq!(out, "same line\nedited one\nsame line\nedited\n");
}

#[test]
fn a_hunk_that_does_not_fit_changes_nothing() {
    let file = "a\nb\nc\n";
    let patch = "*** Begin Patch
*** Update File: a
@@ the second one does not match @@
 a
-b
+ZZZ
 a line that does not exist
*** End Patch
";
    let ops = parse(patch).expect("parse");
    let Op::Update { hunks, .. } = &ops[0] else {
        panic!("should be Update");
    };
    let why = apply(file, hunks).expect_err("a mismatch must be refused");
    assert!(why.contains("matches nothing in the file"), "{why}");
}

#[test]
fn a_hunk_without_context_is_refused() {
    // Only + lines: it knows what to add, not where.
    let why = apply(
        "a\n",
        &[super::patch::Hunk {
            hint: String::new(),
            lines: vec![PatchLine::Add("a new line".to_string())],
        }],
    )
    .expect_err("it should refuse");
    assert!(why.contains("context"), "{why}");
}

#[test]
fn the_trailing_newline_survives() {
    let hunk = super::patch::Hunk {
        hint: String::new(),
        lines: vec![
            PatchLine::Context("a".to_string()),
            PatchLine::Add("b".to_string()),
        ],
    };
    assert_eq!(
        apply("a\n", std::slice::from_ref(&hunk)).expect("the app"),
        "a\nb\n"
    );
    // A file without a trailing newline must not quietly gain one.
    assert_eq!(apply("a", &[hunk]).expect("the app"), "a\nb");
}

// ---- The tool wrapper: paths, gate, all-or-nothing ------------------------------------------

#[tokio::test]
async fn the_tool_writes_two_files_and_creates_one() {
    let ctx = test_ctx().await;
    let home = ctx.home.clone();
    std::fs::write(
        home.join("a.txt"),
        "first line\nthe old content\nthird line\n",
    )
    .expect("seed it");
    let mut reg = ToolRegistry::new();
    reg.register_exclusive(apply_patch(WritePolicy::default()));

    let patch = "*** Begin Patch
*** Update File: a.txt
@@ the middle line @@
 first line
-the old content
+the new content
 third line
*** Add File: sub/b.txt
+(new)'s
*** End Patch
";
    let out = reg
        .execute(ctx.clone(), "apply_patch", json!({ "patch": patch }))
        .await;
    assert!(out.contains("Changed"), "{out}");
    assert!(out.contains("(new)"), "{out}");
    assert_eq!(
        std::fs::read_to_string(home.join("a.txt")).expect("read"),
        "first line\nthe new content\nthird line\n"
    );
    assert_eq!(
        std::fs::read_to_string(home.join("sub/b.txt")).expect("read"),
        "(new)'s\n"
    );
}

#[tokio::test]
async fn one_bad_hunk_leaves_every_file_untouched() {
    let ctx = test_ctx().await;
    let home = ctx.home.clone();
    std::fs::write(home.join("a.txt"), "one\ntwo\n").expect("seed it");
    let mut reg = ToolRegistry::new();
    reg.register_exclusive(apply_patch(WritePolicy::default()));

    // The first file matches and the second does not: the second one voids the whole patch.
    let patch = "*** Begin Patch
*** Update File: a.txt
@@ edit the first place @@
 one
-two
+edited
*** Update File: b.txt
@@ this line does not exist @@
 not there at all
-this line
+that line
*** End Patch
";
    let out = reg
        .execute(ctx.clone(), "apply_patch", json!({ "patch": patch }))
        .await;
    assert!(out.starts_with("Error:"), "{out}");
    assert_eq!(
        std::fs::read_to_string(home.join("a.txt")).expect("read"),
        "one\ntwo\n",
        "one hunk does not match, so no other file may be touched either"
    );
}

#[tokio::test]
async fn add_file_refuses_to_overwrite() {
    let ctx = test_ctx().await;
    let home = ctx.home.clone();
    std::fs::write(home.join("exists.txt"), "leave me alone\n").expect("seed it");
    let mut reg = ToolRegistry::new();
    reg.register_exclusive(apply_patch(WritePolicy::default()));

    let patch = "*** Begin Patch
*** Add File: exists.txt
+overwrite
*** End Patch
";
    let out = reg
        .execute(ctx.clone(), "apply_patch", json!({ "patch": patch }))
        .await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("already exists"), "{out}");
    assert_eq!(
        std::fs::read_to_string(home.join("exists.txt")).expect("read"),
        "leave me alone\n"
    );
}

#[tokio::test]
async fn a_patch_outside_the_workspace_is_refused_before_anything_is_written() {
    let ctx = test_ctx().await;
    let outside = tempfile::tempdir().expect("another directory");
    let mut reg = ToolRegistry::new();
    reg.register_exclusive(apply_patch(WritePolicy::default()));

    let path = outside.path().join("leak.txt");
    let patch = format!(
        "*** Begin Patch\n*** Add File: {}\n+must not reach the disk\n*** End Patch\n",
        path.display()
    );
    let out = reg
        .execute(ctx, "apply_patch", json!({ "patch": patch }))
        .await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(
        !path.exists(),
        "a patch outside the workspace must not reach the disk"
    );
}
