//! `apply_patch`: what it refuses matters more than what it accepts — a half-applied patch gets
//! treated as "done" by whoever reads the diff next.

use serde_json::json;

use super::fs_write::WritePolicy;
use super::patch::{apply, apply_patch, parse, Op, PatchLine};
use crate::{ToolCtx, ToolRegistry};

async fn test_ctx() -> ToolCtx {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    let _ = dir.keep(); // sqlite 还会写 -wal/-shm：这个目录不能被就地删掉
    ToolCtx {
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
+// 新文件
*** End Patch
";

#[test]
fn it_parses_updates_and_adds_in_order() {
    let ops = parse(PATCH).expect("应能解析");
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
        other => panic!("第一个应该是 Update，实际 {other:?}"),
    }
    match &ops[1] {
        Op::Add { path, lines } => {
            assert_eq!(path, "b.rs");
            assert_eq!(lines, &vec!["// 新文件".to_string()]);
        }
        other => panic!("第二个应该是 Add，实际 {other:?}"),
    }
}

#[test]
fn prose_around_the_patch_is_tolerated() {
    let text = format!("这是补丁：\n```diff\n{PATCH}```\n请应用。");
    assert_eq!(parse(&text).expect("包裹应被忽略").len(), 2);
}

#[test]
fn a_missing_end_marker_is_refused() {
    let why = parse("*** Begin Patch\n*** Add File: a\n+x\n").expect_err("半截补丁不该被接受");
    assert!(why.contains("End Patch"), "{why}");
}

#[test]
fn an_unknown_directive_is_refused_not_skipped() {
    // Move 没实现：默默忽略会变成「其余都改了，就是没移动」。
    let why = parse("*** Begin Patch\n*** Move to: b.rs\n*** End Patch\n").expect_err("要拒绝");
    assert!(why.contains("Move"), "{why}");
}

#[test]
fn an_add_file_without_plus_prefix_is_refused() {
    let why = parse("*** Begin Patch\n*** Add File: a\n裸行\n*** End Patch\n").expect_err("要拒绝");
    assert!(why.contains('+'), "{why}");
}

#[test]
fn two_hunks_apply_at_two_different_places() {
    // 同样的两行在文件里出现两次：游标推进后第二个 hunk 落到第二处，而不是又改第一处。
    let file = "同一行\nkeep\n同一行\nkeep\n";
    let patch = "*** Begin Patch
*** Update File: a
@@ one @@
 同一行
-keep
+改了一
@@ two @@
 同一行
-keep
+改了的
*** End Patch
";
    let ops = parse(patch).expect("解析");
    let Op::Update { hunks, .. } = &ops[0] else {
        panic!("应是 Update");
    };
    let out = apply(file, hunks).expect("两处都对得上");
    assert_eq!(out, "同一行\n改了一\n同一行\n改了的\n");
}

#[test]
fn a_hunk_that_does_not_fit_changes_nothing() {
    let file = "a\nb\nc\n";
    let patch = "*** Begin Patch
*** Update File: a
@@ 第二处对不上 @@
 a
-b
+ZZZ
 不存在的行
*** End Patch
";
    let ops = parse(patch).expect("解析");
    let Op::Update { hunks, .. } = &ops[0] else {
        panic!("应是 Update");
    };
    let why = apply(file, hunks).expect_err("对不上就该拒绝");
    assert!(why.contains("找不到对应的原文"), "{why}");
}

#[test]
fn a_hunk_without_context_is_refused() {
    // 只有 + 行：知道加什么，不知道往哪儿加。
    let why = apply(
        "a\n",
        &[super::patch::Hunk {
            hint: String::new(),
            lines: vec![PatchLine::Add("新行".to_string())],
        }],
    )
    .expect_err("要拒绝");
    assert!(why.contains("上下文"), "{why}");
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
        apply("a\n", std::slice::from_ref(&hunk)).expect("应用"),
        "a\nb\n"
    );
    // 原本没有行尾换行就别偷偷加一个。
    assert_eq!(apply("a", &[hunk]).expect("应用"), "a\nb");
}

// ---- The tool wrapper: paths, gate, all-or-nothing ------------------------------------------

#[tokio::test]
async fn the_tool_writes_two_files_and_creates_one() {
    let ctx = test_ctx().await;
    let home = ctx.home.clone();
    std::fs::write(home.join("a.txt"), "第一行\n旧内容\n第三行\n").expect("铺底");
    let mut reg = ToolRegistry::new();
    reg.register_exclusive(apply_patch(WritePolicy::default()));

    let patch = "*** Begin Patch
*** Update File: a.txt
@@ 中间那行 @@
 第一行
-旧内容
+新内容
 第三行
*** Add File: sub/b.txt
+新建的
*** End Patch
";
    let out = reg
        .execute(ctx.clone(), "apply_patch", json!({ "patch": patch }))
        .await;
    assert!(out.contains("已修改"), "{out}");
    assert!(out.contains("新建"), "{out}");
    assert_eq!(
        std::fs::read_to_string(home.join("a.txt")).expect("读"),
        "第一行\n新内容\n第三行\n"
    );
    assert_eq!(
        std::fs::read_to_string(home.join("sub/b.txt")).expect("读"),
        "新建的\n"
    );
}

#[tokio::test]
async fn one_bad_hunk_leaves_every_file_untouched() {
    let ctx = test_ctx().await;
    let home = ctx.home.clone();
    std::fs::write(home.join("a.txt"), "一\n二\n").expect("铺底");
    let mut reg = ToolRegistry::new();
    reg.register_exclusive(apply_patch(WritePolicy::default()));

    // 第一个文件能对上，第二个对不上：第二个让整份补丁作废。
    let patch = "*** Begin Patch
*** Update File: a.txt
@@ 改第一处 @@
 一
-二
+改了
*** Update File: b.txt
@@ 这行不存在 @@
 根本没有
-这行
+那行
*** End Patch
";
    let out = reg
        .execute(ctx.clone(), "apply_patch", json!({ "patch": patch }))
        .await;
    assert!(out.starts_with("Error:"), "{out}");
    assert_eq!(
        std::fs::read_to_string(home.join("a.txt")).expect("读"),
        "一\n二\n",
        "有一处对不上，别的文件也不许动"
    );
}

#[tokio::test]
async fn add_file_refuses_to_overwrite() {
    let ctx = test_ctx().await;
    let home = ctx.home.clone();
    std::fs::write(home.join("exists.txt"), "别动我\n").expect("铺底");
    let mut reg = ToolRegistry::new();
    reg.register_exclusive(apply_patch(WritePolicy::default()));

    let patch = "*** Begin Patch
*** Add File: exists.txt
+覆盖
*** End Patch
";
    let out = reg
        .execute(ctx.clone(), "apply_patch", json!({ "patch": patch }))
        .await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("已经存在"), "{out}");
    assert_eq!(
        std::fs::read_to_string(home.join("exists.txt")).expect("读"),
        "别动我\n"
    );
}

#[tokio::test]
async fn a_patch_outside_the_workspace_is_refused_before_anything_is_written() {
    let ctx = test_ctx().await;
    let outside = tempfile::tempdir().expect("另一个目录");
    let mut reg = ToolRegistry::new();
    reg.register_exclusive(apply_patch(WritePolicy::default()));

    let path = outside.path().join("leak.txt");
    let patch = format!(
        "*** Begin Patch\n*** Add File: {}\n+不该落盘\n*** End Patch\n",
        path.display()
    );
    let out = reg
        .execute(ctx, "apply_patch", json!({ "patch": patch }))
        .await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(!path.exists(), "越界的补丁不该落盘");
}
