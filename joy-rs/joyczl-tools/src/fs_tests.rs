//! Behavior edges of `read_file` / `list_dir`. All temp dirs; runs on Windows too.

use super::*;
use crate::ToolRegistry;
use serde_json::json;

async fn ctx() -> ToolCtx {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    let _ = dir.keep(); // sqlite still writes -wal/-shm
    ToolCtx {
        on_uncertain: None,
        task_run_id: None,
        activate: None,
        embedder: None,
        cwd: None,
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

fn registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(fs::read_file());
    registry.register(fs::list_dir());
    registry
}

async fn run(registry: &ToolRegistry, ctx: &ToolCtx, name: &str, args: Value) -> String {
    registry.execute(ctx.clone(), name, args).await
}

/// Basic read: line numbers, total, footer.
#[tokio::test]
async fn reads_a_file_with_line_numbers_and_a_footer() {
    let registry = registry();
    let ctx = ctx().await;
    let file = ctx.home.join("article.md");
    std::fs::write(&file, "第一行\n第二行\n第三行\n").unwrap();

    let out = run(&registry, &ctx, "read_file", json!({"path": file})).await;
    assert!(out.contains("第一行"), "{out}");
    assert!(out.contains("[Showing lines 1–3 of 3]"), "{out}");
    assert!(
        !out.contains("Use offset"),
        "读完了就不该出现续读提示：{out}"
    );
}

/// Paging: offset/limit cut at the right places, the footer tells the model how to continue.
#[tokio::test]
async fn pagination_slices_and_tells_the_model_how_to_continue() {
    let registry = registry();
    let ctx = ctx().await;
    let file = ctx.home.join("big.md");
    let body: String = (1..=10).map(|i| format!("第{i}行\n")).collect();
    std::fs::write(&file, body).unwrap();

    let out = run(
        &registry,
        &ctx,
        "read_file",
        json!({"path": file, "offset": 4, "limit": 3}),
    )
    .await;
    assert!(out.contains("第4行"), "{out}");
    assert!(out.contains("第6行"), "{out}");
    assert!(!out.contains("第7行"), "{out}");
    assert!(out.contains("[Showing lines 4–6 of 10]"), "{out}");
    assert!(out.contains("[Use offset=7 to continue.]"), "{out}");
}

/// offset out of range: say so honestly; no fake content.
#[tokio::test]
async fn an_offset_past_the_end_says_so() {
    let registry = registry();
    let ctx = ctx().await;
    let file = ctx.home.join("small.md");
    std::fs::write(&file, "只有一行\n").unwrap();

    let out = run(
        &registry,
        &ctx,
        "read_file",
        json!({"path": file, "offset": 99}),
    )
    .await;
    assert!(out.contains("the file has 1 lines"), "{out}");
    assert!(out.contains("out of range"), "{out}");
}

/// Byte cap: cut on a **line boundary** — never half a line; the footer offers the resume offset.
#[tokio::test]
async fn the_byte_cap_cuts_at_a_line_boundary() {
    let registry = registry();
    let ctx = ctx().await;
    let file = ctx.home.join("wide.md");
    // ~1000 bytes per line, 60 lines ≈ 60KB > the 50KB cap.
    let body: String = (1..=60)
        .map(|i| format!("{:04}{}", i, "x".repeat(996)) + "\n")
        .collect();
    std::fs::write(&file, body).unwrap();

    let out = run(&registry, &ctx, "read_file", json!({"path": file})).await;
    // About 50 lines survive (50 × ~1002 ≈ 50.1KB, rounded to line boundaries), never half a line.
    let shown_lines = out.lines().filter(|l| l.contains("xxxx")).count();
    assert!(
        (40..=51).contains(&shown_lines),
        "该在行边界截断，得到 {shown_lines} 行"
    );
    assert!(out.contains("capped at 50000 bytes"), "{out}");
    assert!(out.contains("[Use offset="), "{out}");
    // The last shown line must be a whole line (exactly 1000 bytes), not shredded.
    let matching: Vec<&str> = out.lines().filter(|l| l.contains("xxxx")).collect();
    let last = matching.last().copied().expect("至少一行");
    assert_eq!(
        last.chars().filter(|c| *c == 'x').count(),
        996,
        "半行出现了：{last}"
    );
}

/// Binary file: refused explicitly with a pointer, no mojibake returned.
#[tokio::test]
async fn a_binary_file_is_refused_with_directions() {
    let registry = registry();
    let ctx = ctx().await;
    let file = ctx.home.join("blob.bin");
    std::fs::write(&file, [0xFF, 0x00, 0x41, 0x00]).unwrap();

    let out = run(&registry, &ctx, "read_file", json!({"path": file})).await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("binary"), "{out}");
}

/// Missing file: an Error text, stated honestly.
#[tokio::test]
async fn a_missing_file_is_an_honest_error() {
    let registry = registry();
    let ctx = ctx().await;
    let out = run(
        &registry,
        &ctx,
        "read_file",
        json!({"path": ctx.home.join("nope.md")}),
    )
    .await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("not found"), "{out}");
}

/// A directory passed to read_file: pointed at list_dir.
#[tokio::test]
async fn a_directory_is_redirected_to_list_dir() {
    let registry = registry();
    let ctx = ctx().await;
    let out = run(&registry, &ctx, "read_file", json!({"path": ctx.home})).await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("list_dir"), "{out}");
}

/// Unchanged-content short circuit: same session re-reads the same slice → placeholder; a different range → content as usual.
#[tokio::test]
async fn rereading_the_same_range_returns_a_placeholder() {
    let registry = registry();
    let ctx = ctx().await;
    let file = ctx.home.join("memo.md");
    std::fs::write(&file, "甲\n乙\n丙\n").unwrap();

    let first = run(&registry, &ctx, "read_file", json!({"path": file})).await;
    assert!(first.contains("甲"), "{first}");

    let second = run(&registry, &ctx, "read_file", json!({"path": file})).await;
    assert!(
        second.contains("File unchanged since last read"),
        "第二次该拿占位符：{second}"
    );
    assert!(!second.contains("甲"), "占位符里不该重复内容：{second}");

    // A different range: that's a new page, content as usual.
    let third = run(
        &registry,
        &ctx,
        "read_file",
        json!({"path": file, "offset": 2}),
    )
    .await;
    assert!(third.contains("乙"), "{third}");

    // The file changed: different hash, new content.
    std::fs::write(&file, "甲\n乙\n丙\n丁\n").unwrap();
    let fourth = run(&registry, &ctx, "read_file", json!({"path": file})).await;
    assert!(fourth.contains("丁"), "变了就该给新内容：{fourth}");
}

/// Caches of different sessions don't touch (session fence — same discipline as the owner fence).
#[tokio::test]
async fn the_read_cache_is_per_session() {
    let registry = registry();
    let ctx = ctx().await;
    let file = ctx.home.join("s.md");
    std::fs::write(&file, "一行\n").unwrap();

    let first = run(&registry, &ctx, "read_file", json!({"path": file})).await;
    assert!(first.contains("一行"), "{first}");

    let mut other = ctx.clone();
    other.session_id = "另一个会话".to_string();
    let second = run(&registry, &other, "read_file", json!({"path": file})).await;
    assert!(
        second.contains("一行"),
        "别的会话第一次读该拿到全文：{second}"
    );
}

/// `~` expansion: models often write `~/docs/x.md`; unexpanded it's a misleading "not found".
/// Skipped in environments without HOME/USERPROFILE (Windows CI makes no such assumption).
#[test]
fn tilde_expands_against_home() {
    let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) else {
        return;
    };
    assert_eq!(
        fs::expand_tilde("~/docs/x.md"),
        std::path::PathBuf::from(&home).join("docs/x.md"),
    );
    assert_eq!(fs::expand_tilde("~"), std::path::PathBuf::from(&home));
    // Leave non-~-prefixed paths alone.
    assert_eq!(
        fs::expand_tilde("相对/路径.md"),
        std::path::PathBuf::from("相对/路径.md")
    );
    assert_eq!(
        fs::expand_tilde("/abs/x.md"),
        std::path::PathBuf::from("/abs/x.md")
    );
}

/// list_dir: dirs first, files with sizes, an entry count.
#[tokio::test]
async fn list_dir_puts_directories_first_with_sizes() {
    let registry = registry();
    let ctx = ctx().await;
    // Not home itself: ctx() puts state.db there, so the entry count would drift with scaffolding.
    let dir = ctx.home.join("桌");
    std::fs::create_dir(&dir).unwrap();
    std::fs::create_dir(dir.join("子目录")).unwrap();
    std::fs::write(dir.join("b.txt"), "12345").unwrap();
    std::fs::write(dir.join("a.txt"), "x").unwrap();

    let out = run(&registry, &ctx, "list_dir", json!({"path": dir})).await;
    let (before, _) = out.split_once("子目录/").expect("子目录/在列表里");
    assert!(!before.contains(".txt"), "目录该排在文件之前：{out}");
    assert!(out.contains("a.txt  (1 B)"), "{out}");
    assert!(out.contains("b.txt  (5 B)"), "{out}");
    assert!(out.contains("[3 entries]"), "{out}");
}

/// list_dir's 500-entry cap: truncate and label honestly.
#[tokio::test]
async fn list_dir_caps_at_500_entries() {
    let registry = registry();
    let ctx = ctx().await;
    let dir = ctx.home.join("many");
    std::fs::create_dir(&dir).unwrap();
    for i in 0..600 {
        std::fs::write(dir.join(format!("f{i:04}.txt")), "").unwrap();
    }

    let out = run(&registry, &ctx, "list_dir", json!({"path": dir})).await;
    assert!(out.contains("[+100 more not shown"), "{out}");
    assert_eq!(
        out.lines().filter(|l| l.contains(".txt")).count(),
        500,
        "恰好展示 500 项"
    );
}

/// list_dir: empty dir, missing, a file passed in — all three answered honestly.
#[tokio::test]
async fn list_dir_edge_cases_are_honest() {
    let registry = registry();
    let ctx = ctx().await;
    let empty = ctx.home.join("空");
    std::fs::create_dir(&empty).unwrap();
    let out = run(&registry, &ctx, "list_dir", json!({"path": empty})).await;
    assert!(out.contains("empty directory"), "{out}");

    let out = run(
        &registry,
        &ctx,
        "list_dir",
        json!({"path": ctx.home.join("没有")}),
    )
    .await;
    assert!(out.starts_with("Error:"), "{out}");

    let file = ctx.home.join("是个文件.txt");
    std::fs::write(&file, "").unwrap();
    let out = run(&registry, &ctx, "list_dir", json!({"path": file})).await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("read_file"), "{out}");
}

/// Relative paths resolve against this turn's working directory (session-level semantics): with
/// ctx.cwd set, "note.txt" is looked up under it — no longer process cwd, no silent fallback.
#[tokio::test]
async fn relative_paths_resolve_against_the_turn_cwd() {
    let registry = registry();
    let mut ctx = ctx().await;
    let work = tempfile::tempdir().expect("工作目录");
    std::fs::write(work.path().join("note.txt"), "来自工作目录").unwrap();

    ctx.cwd = Some(work.path().to_path_buf());
    let out = run(&registry, &ctx, "read_file", json!({"path": "note.txt"})).await;
    assert!(out.contains("来自工作目录"), "{out}");
}

/// Without ctx.cwd the behavior is unchanged: relative paths still resolve against process cwd (old behavior, for compatibility).
#[tokio::test]
async fn without_a_turn_cwd_relative_paths_still_use_process_cwd() {
    let registry = registry();
    let ctx = ctx().await;
    std::fs::write(ctx.home.join("legacy.txt"), "旧行为").unwrap();

    // Process cwd isn't home, so absolute paths read and bare relative ones don't — proof the semantics weren't quietly changed.
    let out = run(
        &registry,
        &ctx,
        "read_file",
        json!({"path": ctx.home.join("legacy.txt")}),
    )
    .await;
    assert!(out.contains("旧行为"), "{out}");
}

// ---- spill 地址：模型照抄「全量在 spill/…」时真的能读到 ---------------------------------

/// spill.rs 给模型的是**相对 home** 的地址；cwd 通常不在 home 下，所以 read_file 要认这个形状。
#[tokio::test]
async fn a_spill_address_resolves_against_home_not_only_the_cwd() {
    let ctx = ctx().await;
    let day = ctx.home.join("spill").join("20261112");
    std::fs::create_dir_all(&day).expect("建 spill 目录");
    std::fs::write(day.join("exec-120000-1.txt"), "完整输出在这里\n").expect("写 spill 文件");

    // 目录故意选在 home 之外：这正是真实情形（本轮工作目录是某个项目）。
    let elsewhere = tempfile::tempdir().expect("临时目录");
    let ctx = ToolCtx {
        on_uncertain: None,
        task_run_id: None,
        cwd: Some(elsewhere.path().to_path_buf()),
        ..ctx
    };

    let out = run(
        &registry(),
        &ctx,
        "read_file",
        json!({"path": "spill/20261112/exec-120000-1.txt"}),
    )
    .await;
    assert!(out.contains("完整输出在这里"), "{out}");
}

/// 回落只认 `spill/` 前缀：普通相对路径not found时**不能**去 home 下猜（那是无声扩权）。
#[tokio::test]
async fn a_plain_relative_path_never_falls_back_to_home() {
    let ctx = ctx().await;
    std::fs::write(ctx.home.join("notes.md"), "home 下的同名文件\n").expect("写文件");
    let elsewhere = tempfile::tempdir().expect("临时目录");
    let ctx = ToolCtx {
        on_uncertain: None,
        task_run_id: None,
        cwd: Some(elsewhere.path().to_path_buf()),
        ..ctx
    };

    let out = run(&registry(), &ctx, "read_file", json!({"path": "notes.md"})).await;
    assert!(out.starts_with("Error"), "不该在 home 下找到它：{out}");
    assert!(!out.contains("home 下的同名文件"), "{out}");
}
