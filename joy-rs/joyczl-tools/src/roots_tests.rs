//! The writable-boundary rule: one definition for the write tools and the exec sandbox.
//!
//! These tests pin the **refusals** — the part that used to disagree between the two
//! consumers — plus the one deliberate exception (a folder the human picked is honoured
//! even when it happens to be `<home>`).

use std::path::{Path, PathBuf};

use super::roots::{sandbox_roots, HOME_CONTROL};

fn dirs() -> (tempfile::TempDir, tempfile::TempDir) {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    (home, workspace)
}

fn paths(items: &[&Path]) -> Vec<PathBuf> {
    items.iter().map(|p| p.to_path_buf()).collect()
}

#[test]
fn a_plain_workspace_is_kept() {
    let (home, workspace) = dirs();
    let roots = sandbox_roots(Some(workspace.path()), &[], home.path());
    assert_eq!(roots.workspaces, vec![workspace.path().to_path_buf()]);
    assert!(roots.dropped.is_empty());
}

#[test]
fn the_home_is_never_an_extra_root() {
    let (home, workspace) = dirs();
    let extra = paths(&[home.path(), workspace.path()]);
    let roots = sandbox_roots(Some(workspace.path()), &extra, home.path());
    assert_eq!(
        roots.workspaces,
        vec![workspace.path().to_path_buf()],
        "home has to be dropped, the working directory stays"
    );
    assert_eq!(roots.dropped, vec![home.path().to_path_buf()]);
}

#[test]
fn an_ancestor_of_home_is_never_an_extra_root() {
    let (home, _) = dirs();
    let parent = home.path().parent().expect("a tempdir has a parent");
    let roots = sandbox_roots(None, &paths(&[parent]), home.path());
    assert!(
        roots.workspaces.is_empty(),
        "home's parent directory hands over the control plane too: {:?}",
        roots.workspaces
    );
    assert_eq!(roots.dropped.len(), 1);
}

#[test]
fn the_disk_root_is_never_a_root() {
    let (home, _) = dirs();
    let as_extra = sandbox_roots(None, &paths(&[Path::new("/")]), home.path());
    assert!(
        as_extra.workspaces.is_empty(),
        "/ cannot be an extra writable root"
    );
    let as_cwd = sandbox_roots(Some(Path::new("/")), &[], home.path());
    assert!(
        as_cwd.workspaces.is_empty(),
        "/ cannot be a working directory either"
    );
}

#[test]
fn every_control_plane_entry_is_refused() {
    let (home, _) = dirs();
    for entry in HOME_CONTROL {
        let target = home.path().join(entry);
        let roots = sandbox_roots(None, std::slice::from_ref(&target), home.path());
        assert!(
            roots.workspaces.is_empty(),
            "a control-plane entry ({entry}) must not be opened up"
        );
        assert_eq!(roots.dropped, vec![target.clone()], "{entry}");
    }
}

#[test]
fn a_normal_subdirectory_of_home_can_be_opted_in() {
    // A human who really needs a shell to write somewhere under home can name that
    // directory explicitly — the escape hatch exists, it just isn't the whole home.
    let (home, _) = dirs();
    let cache = home.path().join("build-cache");
    std::fs::create_dir(&cache).expect("create the directory");
    let roots = sandbox_roots(None, std::slice::from_ref(&cache), home.path());
    assert_eq!(roots.workspaces, vec![cache]);
}

#[test]
fn a_chosen_workspace_that_happens_to_be_home_is_honoured() {
    // The folder picker said "this one". That is an authorization, not a config typo, so it
    // is not filtered — the extra-root check exists to catch mistakes, not decisions.
    let (home, _) = dirs();
    let roots = sandbox_roots(Some(home.path()), &[], home.path());
    assert_eq!(roots.workspaces, vec![home.path().to_path_buf()]);
    assert!(roots.dropped.is_empty());
}

#[test]
fn duplicate_roots_collapse_to_one_and_keep_order() {
    let (home, workspace) = dirs();
    let extra = paths(&[workspace.path(), home.path(), workspace.path()]);
    let roots = sandbox_roots(Some(workspace.path()), &extra, home.path());
    assert_eq!(roots.workspaces, vec![workspace.path().to_path_buf()]);
}

#[test]
fn the_sandbox_and_the_write_tools_share_one_list() {
    let (home, workspace) = dirs();
    let cache = home.path().join("build-cache");
    let extra: Vec<PathBuf> = vec![cache.clone(), workspace.path().to_path_buf()];
    let with_cwd = sandbox_roots(Some(workspace.path()), &extra, home.path());
    let listed = sandbox_roots(
        None,
        &[workspace.path().to_path_buf(), cache.clone()],
        home.path(),
    );
    assert_eq!(
        with_cwd.workspaces, listed.workspaces,
        "cwd and extra roots follow one rule; two consumers must not produce two sets of bounds"
    );
    assert!(
        with_cwd
            .workspaces
            .iter()
            .all(|p| !p.starts_with(home.path()) || p == &cache),
        "under home only the explicitly listed subdirectory is allowed"
    );
}

#[test]
fn an_empty_home_still_refuses_the_disk_root() {
    // A hand-built policy with no home (tests, embedders) must not silently allow `/`.
    let roots = sandbox_roots(None, &paths(&[Path::new("/")]), Path::new(""));
    assert!(roots.workspaces.is_empty());
}
