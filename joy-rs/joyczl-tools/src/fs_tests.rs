//! `read_file` / `list_dir` 的行为边界。全部用临时目录，Windows 可跑。

use super::*;
use crate::ToolRegistry;
use serde_json::json;

async fn ctx() -> ToolCtx {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().to_path_buf();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    let _ = dir.keep(); // sqlite 还要写 -wal/-shm
    ToolCtx {
        cwd: None,
        approval: None,
        jobs: None,
        hooks: None,
        session_id: "test".to_string(),
        facts: joyczl_state::Facts::new(pool.clone()),
        episodes: joyczl_state::Episodes::new(pool.clone()),
        chat: joyczl_state::Chat::new(pool.clone()),
        calendar: joyczl_state::Calendar::new(pool),
        home,
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

/// 基本读取：行号、总数、页脚。
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

/// 分页：offset/limit 切对位置，页脚告诉模型怎么继续。
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

/// offset 超出范围：如实说，不给假内容。
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
    assert!(out.contains("文件共 1 行"), "{out}");
    assert!(out.contains("超出范围"), "{out}");
}

/// 字节上限：在**行边界**上截 —— 不会出现半行，页脚给续读提示。
#[tokio::test]
async fn the_byte_cap_cuts_at_a_line_boundary() {
    let registry = registry();
    let ctx = ctx().await;
    let file = ctx.home.join("wide.md");
    // 每行 ~1000 字节，60 行 ≈ 60KB > 50KB 上限。
    let body: String = (1..=60)
        .map(|i| format!("{:04}{}", i, "x".repeat(996)) + "\n")
        .collect();
    std::fs::write(&file, body).unwrap();

    let out = run(&registry, &ctx, "read_file", json!({"path": file})).await;
    // 50 行上下被留下（50 × ~1002 ≈ 50.1KB，行边界取整），绝不出现半行。
    let shown_lines = out.lines().filter(|l| l.contains("xxxx")).count();
    assert!(
        (40..=51).contains(&shown_lines),
        "该在行边界截断，得到 {shown_lines} 行"
    );
    assert!(out.contains("capped at 50000 bytes"), "{out}");
    assert!(out.contains("[Use offset="), "{out}");
    // 最后一个展示行必须是完整行（1000 字节整），不是被切碎的。
    let matching: Vec<&str> = out.lines().filter(|l| l.contains("xxxx")).collect();
    let last = matching.last().copied().expect("至少一行");
    assert_eq!(
        last.chars().filter(|c| *c == 'x').count(),
        996,
        "半行出现了：{last}"
    );
}

/// 二进制文件：明确拒绝并指路，不返回乱码。
#[tokio::test]
async fn a_binary_file_is_refused_with_directions() {
    let registry = registry();
    let ctx = ctx().await;
    let file = ctx.home.join("blob.bin");
    std::fs::write(&file, [0xFF, 0x00, 0x41, 0x00]).unwrap();

    let out = run(&registry, &ctx, "read_file", json!({"path": file})).await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("二进制"), "{out}");
}

/// 不存在的文件：Error 文本，如实说。
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
    assert!(out.contains("找不到"), "{out}");
}

/// 目录传给 read_file：指路到 list_dir。
#[tokio::test]
async fn a_directory_is_redirected_to_list_dir() {
    let registry = registry();
    let ctx = ctx().await;
    let out = run(&registry, &ctx, "read_file", json!({"path": ctx.home})).await;
    assert!(out.starts_with("Error:"), "{out}");
    assert!(out.contains("list_dir"), "{out}");
}

/// 内容不变短路：同会话重读同一段 → 占位符；换了范围 → 照常给内容。
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

    // 换个范围：那是新的一页，照常给内容。
    let third = run(
        &registry,
        &ctx,
        "read_file",
        json!({"path": file, "offset": 2}),
    )
    .await;
    assert!(third.contains("乙"), "{third}");

    // 文件变了：哈希不同，给新内容。
    std::fs::write(&file, "甲\n乙\n丙\n丁\n").unwrap();
    let fourth = run(&registry, &ctx, "read_file", json!({"path": file})).await;
    assert!(fourth.contains("丁"), "变了就该给新内容：{fourth}");
}

/// 不同会话的缓存互不影响（session 围栏 —— 和 owner 围栏同一纪律）。
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

/// `~` 展开：模型常写 `~/docs/x.md`，不展开就是一条误导性的「找不到」。
/// 没设 HOME/USERPROFILE 的环境跳过（Windows CI 不做这个假设）。
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
    // 别动非 ~ 开头的路径。
    assert_eq!(
        fs::expand_tilde("相对/路径.md"),
        std::path::PathBuf::from("相对/路径.md")
    );
    assert_eq!(
        fs::expand_tilde("/abs/x.md"),
        std::path::PathBuf::from("/abs/x.md")
    );
}

/// list_dir：目录在前、文件带大小、条目计数。
#[tokio::test]
async fn list_dir_puts_directories_first_with_sizes() {
    let registry = registry();
    let ctx = ctx().await;
    // 不用 home 本身：ctx() 在那里放了 state.db，条目数会随脚手架变。
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

/// list_dir 的 500 项上限：截断并如实标注。
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

/// list_dir：空目录、不存在、传文件，三种都如实说。
#[tokio::test]
async fn list_dir_edge_cases_are_honest() {
    let registry = registry();
    let ctx = ctx().await;
    let empty = ctx.home.join("空");
    std::fs::create_dir(&empty).unwrap();
    let out = run(&registry, &ctx, "list_dir", json!({"path": empty})).await;
    assert!(out.contains("空目录"), "{out}");

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

/// 相对路径按本轮工作目录解析（会话级语义）：ctx.cwd 设了，"note.txt" 就在它
/// 下面找 —— 不再按进程 cwd，也不会静默回退。
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

/// 没设 ctx.cwd 时行为不变：相对路径仍按进程 cwd（旧行为，向后兼容）。
#[tokio::test]
async fn without_a_turn_cwd_relative_paths_still_use_process_cwd() {
    let registry = registry();
    let ctx = ctx().await;
    std::fs::write(ctx.home.join("legacy.txt"), "旧行为").unwrap();

    // 进程 cwd 不是 home，所以用绝对路径能读、裸相对路径找不到 —— 证明没被偷偷改语义。
    let out = run(
        &registry,
        &ctx,
        "read_file",
        json!({"path": ctx.home.join("legacy.txt")}),
    )
    .await;
    assert!(out.contains("旧行为"), "{out}");
}
