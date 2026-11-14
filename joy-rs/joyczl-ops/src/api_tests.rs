//! Tests one thing only: **which notifications belong to this turn**.
//!
//! The broadcast reaches all subscribers, and app-server runs everyone's sessions in one
//! process — when another session starts on the gateway, its `textDelta` also flows onto
//! the dashboard's connection. A wrong filter mixes someone else's words in; pin it here.

use serde_json::json;

use super::belongs;
use joyczl_protocol::ServerNotification;

fn note(value: serde_json::Value) -> ServerNotification {
    serde_json::from_value(value).expect("a test notification")
}

fn started(session: &str, turn: &str) -> ServerNotification {
    note(json!({
        "type": "turnStarted",
        "turnId": turn,
        "sessionId": session,
        "userMessage": "hello",
        "ts": "2026-01-01T00:00:00+08:00",
    }))
}

fn delta(turn: &str) -> ServerNotification {
    note(json!({"type": "textDelta", "turnId": turn, "delta": "char"}))
}

#[test]
fn the_first_frame_is_known_by_session_the_rest_by_turn() {
    let opened = started("s1", "t1");
    assert_eq!(belongs(&opened, "s1", None).as_deref(), Some("t1"));

    // Once this turn is claimed, everything with the same id passes.
    assert_eq!(
        belongs(&delta("t1"), "s1", Some("t1")).as_deref(),
        Some("t1")
    );
    // Another turn's frame is dropped — even with the same session id (happens when two run concurrently).
    assert_eq!(belongs(&delta("t2"), "s1", Some("t1")), None);
}

#[test]
fn another_sessions_notifications_do_not_leak_in() {
    assert_eq!(belongs(&started("s2", "t9"), "s1", None), None);
    // Before this turn is claimed, nothing is forwarded.
    assert_eq!(belongs(&delta("t9"), "s1", None), None);
}

#[test]
fn a_notification_without_a_turn_id_is_forwarded_once_claimed() {
    // consolidationCompleted and graph* carry no turnId: they belong to the session's turn.
    let consolidation = note(json!({"type": "consolidationCompleted", "newFacts": 2}));
    assert_eq!(
        belongs(&consolidation, "s1", Some("t1")).as_deref(),
        Some("t1")
    );
    // But before anything is claimed, they likewise have nowhere to go.
    assert_eq!(belongs(&consolidation, "s1", None), None);
}

/// Extension whitelist: only 1-8 alphanumerics pass; double extensions look at the last
/// segment; path shapes, CJK and overlong all rejected — the rest falls to `bin` (no error).
#[test]
fn upload_extension_whitelist_is_strict() {
    use super::safe_extension;

    assert_eq!(safe_extension("a.txt").as_deref(), Some("txt"));
    assert_eq!(safe_extension("report.PDF").as_deref(), Some("pdf"));
    assert_eq!(safe_extension("no_ext").as_deref(), None, "no extension");
    assert_eq!(
        safe_extension("x.tar.gz").as_deref(),
        Some("gz"),
        "only the last segment"
    );
    assert_eq!(
        safe_extension("../../etc/passwd").as_deref(),
        None,
        "a path-shaped name does not get in"
    );
    assert_eq!(safe_extension("a.with-cjk").as_deref(), None);
    assert_eq!(
        safe_extension("a.0123456789").as_deref(),
        None,
        "more than 8 digits"
    );
}

/// Directory browsing: lists only subdirectories, sorted by name, hides dot-prefixed and
/// build noise; relative and nonexistent paths get an actionable message.
#[test]
fn fs_browsing_lists_only_directories_and_hides_noise() {
    use super::list_dirs;

    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    std::fs::create_dir(root.join("project alpha")).unwrap();
    std::fs::create_dir(root.join("beta")).unwrap();
    std::fs::write(root.join("file.txt"), "not a directory").unwrap();
    std::fs::create_dir(root.join(".hidden")).unwrap();
    std::fs::create_dir(root.join("target")).unwrap();

    let listing = list_dirs(&root.to_string_lossy(), false).expect("it lists");
    let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["beta", "project alpha"],
        "subdirectories only, sorted, no dotfiles and no target"
    );
    assert_eq!(
        listing.parent.as_deref(),
        Some(root.parent().unwrap().to_string_lossy().as_ref())
    );

    assert!(
        list_dirs("relative/path", false).is_err(),
        "a relative path is refused"
    );
    assert!(list_dirs("/no/such/directory/joy", false).is_err());
}

/// Hidden-directory switch: dot-prefixed hidden by default, shown only when asked;
/// build noise never shows.
#[test]
fn hidden_directories_show_only_when_asked() {
    use super::list_dirs;

    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::create_dir(root.join("node_modules")).unwrap();
    std::fs::create_dir(root.join("normal")).unwrap();

    let hidden_off = list_dirs(&root.to_string_lossy(), false).expect("it lists");
    let names: Vec<&str> = hidden_off.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["normal"],
        "ordinary directories only by default"
    );

    let hidden_on = list_dirs(&root.to_string_lossy(), true).expect("it lists");
    let names: Vec<&str> = hidden_on.entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&".git"), "with the switch on, dotfiles show");
    assert!(!names.contains(&"node_modules"), "build noise never shows");
}

/// New folder: valid names can be created; path shapes (separators, .., dot prefix),
/// overlong names, duplicates and bad parents are all blocked — user input never enters
/// the path shape, same discipline as upload's extension whitelist.
#[test]
fn folder_creation_rejects_path_shapes() {
    use super::create_subdir;

    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().to_string_lossy().to_string();

    let created = create_subdir(&root, "a new project").expect("it can be built");
    assert!(std::path::Path::new(&created).is_dir());

    for bad in [
        "a/b",
        "a\\b",
        "..",
        "x/../y",
        ".hidden",
        "",
        "x".repeat(65).as_str(),
    ] {
        assert!(
            create_subdir(&root, bad).is_err(),
            "should be kept out: {bad:?}"
        );
    }
    assert!(
        create_subdir(&root, "a new project").is_err(),
        "a duplicate name is kept out"
    );
    assert!(
        create_subdir("/no/such/joy", "x").is_err(),
        "a bad parent directory is kept out"
    );
}
