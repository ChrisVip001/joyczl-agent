//! Project instructions: discovery order, precedence, and the budget.

use std::fs;
use std::path::Path;

use super::{discover, MAX_BYTES};

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temporary directory");
    fs::create_dir(dir.path().join(".git")).expect("建 .git");
    dir
}

fn write(dir: &Path, name: &str, text: &str) {
    fs::write(dir.join(name), text).expect("写文件");
}

#[test]
fn nothing_anywhere_is_none_and_costs_nothing() {
    let repo = repo();
    assert_eq!(
        discover(repo.path()),
        None,
        "没有项目文件就不该有任何 section"
    );
}

#[test]
fn one_file_at_the_root_is_read() {
    let repo = repo();
    write(repo.path(), "AGENTS.md", "用 pnpm，不要用 npm。");
    let text = discover(repo.path()).expect("应该有");
    assert!(text.contains("用 pnpm"), "{text}");
    assert!(text.contains("AGENTS.md"), "要标明来源：{text}");
}

#[test]
fn the_nested_file_is_appended_after_the_root_one() {
    let repo = repo();
    let sub = repo.path().join("packages/api");
    fs::create_dir_all(&sub).expect("建子目录");
    write(repo.path(), "AGENTS.md", "根约定");
    write(&sub, "AGENTS.md", "子目录约定");

    let text = discover(&sub).expect("应该有");
    let root_at = text.find("根约定").expect("根约定");
    let sub_at = text.find("子目录约定").expect("子目录约定");
    assert!(root_at < sub_at, "外层在前、内层在后（后读的赢）：{text}");
}

#[test]
fn override_wins_over_plain_and_plain_wins_over_claude() {
    let repo = repo();
    write(repo.path(), "AGENTS.md", "普通版");
    write(repo.path(), "AGENTS.override.md", "覆盖版");
    write(repo.path(), "CLAUDE.md", "别家版");

    let text = discover(repo.path()).expect("应该有");
    assert!(text.contains("覆盖版"), "{text}");
    assert!(!text.contains("普通版"), "每层只取一个：{text}");
    assert!(!text.contains("别家版"), "每层只取一个：{text}");
}

#[test]
fn claude_md_is_the_last_resort() {
    let repo = repo();
    write(repo.path(), "CLAUDE.md", "别家版");
    let text = discover(repo.path()).expect("应该有");
    assert!(text.contains("别家版"), "{text}");
}

#[test]
fn walking_stops_at_the_repository_root() {
    let outer = tempfile::tempdir().expect("外层");
    write(outer.path(), "AGENTS.md", "不该被读到");
    let repo = outer.path().join("proj");
    fs::create_dir(&repo).expect("建 repo");
    fs::create_dir(repo.join(".git")).expect("建 .git");
    write(&repo, "AGENTS.md", "项目自己的");

    let text = discover(&repo).expect("应该有");
    assert!(text.contains("项目自己的"), "{text}");
    assert!(
        !text.contains("不该被读到"),
        ".git 之上属于别的项目：{text}"
    );
}

#[test]
fn the_budget_drops_the_outer_file_not_the_inner_one() {
    let repo = repo();
    let sub = repo.path().join("inner");
    fs::create_dir(&sub).expect("建子目录");
    write(repo.path(), "AGENTS.md", &"外".repeat(MAX_BYTES));
    write(&sub, "AGENTS.md", "内层必须留下");

    let text = discover(&sub).expect("应该有");
    assert!(text.contains("内层必须留下"), "预算先保内层：{text}");
    assert!(!text.contains(&"外".repeat(100)), "外层被丢掉：{text}");
}

#[test]
fn an_oversized_inner_file_is_truncated_on_a_char_boundary() {
    let repo = repo();
    write(repo.path(), "AGENTS.md", &"字".repeat(MAX_BYTES));
    let text = discover(repo.path()).expect("应该有");
    assert!(text.contains("the tail was cut"), "截断要说明：{text}");
    // 不该出现替换字符：截断点落在字符边界上
    assert!(!text.contains('\u{FFFD}'), "{text}");
}

#[test]
fn an_empty_file_is_skipped_entirely() {
    let repo = repo();
    write(repo.path(), "AGENTS.md", "   \n\n");
    assert_eq!(discover(repo.path()), None);
}

#[test]
fn a_missing_working_directory_is_none() {
    assert_eq!(discover(Path::new("/definitely/not/here")), None);
}
