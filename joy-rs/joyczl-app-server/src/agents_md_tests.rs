//! Project instructions: discovery order, precedence, and the budget.

use std::fs;
use std::path::Path;

use super::{discover, MAX_BYTES};

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temporary directory");
    fs::create_dir(dir.path().join(".git")).expect("create .git");
    dir
}

fn write(dir: &Path, name: &str, text: &str) {
    fs::write(dir.join(name), text).expect("write the file");
}

#[test]
fn nothing_anywhere_is_none_and_costs_nothing() {
    let repo = repo();
    assert_eq!(
        discover(repo.path()),
        None,
        "with no project file there should be no section at all"
    );
}

#[test]
fn one_file_at_the_root_is_read() {
    let repo = repo();
    write(repo.path(), "AGENTS.md", "use pnpm, not npm.");
    let text = discover(repo.path()).expect("it should be found");
    assert!(text.contains("use pnpm"), "{text}");
    assert!(
        text.contains("AGENTS.md"),
        "the source has to be named: {text}"
    );
}

#[test]
fn the_nested_file_is_appended_after_the_root_one() {
    let repo = repo();
    let sub = repo.path().join("packages/api");
    fs::create_dir_all(&sub).expect("create the subdirectory");
    write(repo.path(), "AGENTS.md", "root rules");
    write(&sub, "AGENTS.md", "subdirectory rules");

    let text = discover(&sub).expect("it should be found");
    let root_at = text.find("root rules").expect("the root rules");
    let sub_at = text
        .find("subdirectory rules")
        .expect("the subdirectory rules");
    assert!(
        root_at < sub_at,
        "outer first, inner after (the later one wins): {text}"
    );
}

#[test]
fn override_wins_over_plain_and_plain_wins_over_claude() {
    let repo = repo();
    write(repo.path(), "AGENTS.md", "plain edition");
    write(repo.path(), "AGENTS.override.md", "override edition");
    write(repo.path(), "CLAUDE.md", "someone else's edition");

    let text = discover(repo.path()).expect("it should be found");
    assert!(text.contains("override edition"), "{text}");
    assert!(!text.contains("plain edition"), "one per level: {text}");
    assert!(
        !text.contains("someone else's edition"),
        "one per level: {text}"
    );
}

#[test]
fn claude_md_is_the_last_resort() {
    let repo = repo();
    write(repo.path(), "CLAUDE.md", "someone else's edition");
    let text = discover(repo.path()).expect("it should be found");
    assert!(text.contains("someone else's edition"), "{text}");
}

#[test]
fn walking_stops_at_the_repository_root() {
    let outer = tempfile::tempdir().expect("the outer directory");
    write(outer.path(), "AGENTS.md", "must not be read");
    let repo = outer.path().join("proj");
    fs::create_dir(&repo).expect("create the repo");
    fs::create_dir(repo.join(".git")).expect("create .git");
    write(&repo, "AGENTS.md", "the project's own");

    let text = discover(&repo).expect("it should be found");
    assert!(text.contains("the project's own"), "{text}");
    assert!(
        !text.contains("must not be read"),
        "above .git belongs to another project: {text}"
    );
}

#[test]
fn the_budget_drops_the_outer_file_not_the_inner_one() {
    let repo = repo();
    let sub = repo.path().join("inner");
    fs::create_dir(&sub).expect("create the subdirectory");
    write(repo.path(), "AGENTS.md", &"x".repeat(MAX_BYTES));
    write(&sub, "AGENTS.md", "the inner layer must survive");

    let text = discover(&sub).expect("it should be found");
    assert!(
        text.contains("the inner layer must survive"),
        "the budget protects the inner layer first: {text}"
    );
    assert!(
        !text.contains(&"x".repeat(100)),
        "the outer layer is dropped: {text}"
    );
}

#[test]
fn an_oversized_inner_file_is_truncated_on_a_char_boundary() {
    let repo = repo();
    write(repo.path(), "AGENTS.md", &"char".repeat(MAX_BYTES));
    let text = discover(repo.path()).expect("it should be found");
    assert!(
        text.contains("the tail was cut"),
        "a cut has to be declared: {text}"
    );
    // No replacement character may appear: the cut lands on a char boundary
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
