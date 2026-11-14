//! Tests for the capability center: which rows a human may manage, and the write fences.
//!
//! The listing is a merge of the same files the runtime reads, and every write must
//! refuse anything built-in — these pin both halves, plus the one fence that matters
//! most: **a name owned by a built-in pack is refused, not silently shadowed**.

use std::path::{Path, PathBuf};

use joyczl_mcp::config::ServerSpec;
use serde_json::json;

use crate::capabilities::{
    mcp_rows_in, remove_server, remove_skill, save_server_among, skill_rows_in, validate_server,
};

/// Write `<dir>/<name>/SKILL.md`.
fn put_skill(dir: &Path, name: &str, description: &str) {
    let folder = dir.join(name);
    std::fs::create_dir_all(&folder).expect("create the directory");
    std::fs::write(
        folder.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\nthe body\n"),
    )
    .expect("write the skill");
}

#[test]
fn skills_carry_where_they_came_from_and_home_wins() {
    let home = tempfile::tempdir().expect("temp dir");
    let user = home.path().join("skills");
    let pack = home.path().join("pack");
    std::fs::create_dir_all(&user).expect("create the user directory");
    std::fs::create_dir_all(&pack).expect("create the built-in pack");

    put_skill(&user, "mine", "written by me");
    put_skill(&pack, "bundled", "shipped with Joy");
    // Same name in both: the user's copy wins — and the row says so.
    put_skill(&pack, "shared", "the version in the built-in pack");
    put_skill(&user, "shared", "the version I edited myself");
    // The staging area is not a skill, and a SKILL.md in the root is not either.
    std::fs::create_dir_all(pack.join("_incoming")).expect("create the staging area");
    std::fs::write(
        pack.join("_incoming/SKILL.md"),
        "---\nname: x\ndescription: y\n---\n",
    )
    .expect("write");
    std::fs::write(
        pack.join("SKILL.md"),
        "---\nname: root\ndescription: y\n---\n",
    )
    .expect("write");

    let (rows, error) = skill_rows_in(&[user.clone(), pack.clone()]);
    assert!(
        error.is_none(),
        "there should be nothing to report: {error:?}"
    );
    let by_name = |name: &str| {
        rows.iter()
            .find(|row| row.name == name)
            .expect("this row should be here")
    };

    assert_eq!(by_name("mine").source, "user");
    assert_eq!(by_name("bundled").source, "builtin");
    // The shared one is the user's row — same priority the loader uses.
    assert_eq!(by_name("shared").source, "user");
    assert_eq!(by_name("shared").description, "the version I edited myself");
    assert_eq!(
        rows.len(),
        3,
        "what sits in staging and the root does not count: {rows:?}"
    );
}

#[test]
fn a_missing_user_directory_is_not_an_error() {
    let home = tempfile::tempdir().expect("temp dir");
    let (rows, error) = skill_rows_in(&[home.path().join("skills"), home.path().join("pack")]);
    assert!(rows.is_empty());
    assert!(
        error.is_none(),
        "never having made a skills directory is the default on a fresh install, not an error"
    );
}

#[test]
fn only_user_skills_can_be_deleted() {
    let home = tempfile::tempdir().expect("temp dir");
    let user = home.path().join("skills");
    let pack = home.path().join("pack");
    std::fs::create_dir_all(&user).expect("create the directory");
    put_skill(&user, "mine", "mine");
    put_skill(&pack, "bundled", "the built-in one");

    // The built-in one does not live under home/skills — refused, not "not found".
    let refusal =
        remove_skill(home.path(), "bundled").expect_err("a built-in skill cannot be deleted");
    assert!(refusal.contains("built-in"), "{refusal}");

    // A traversal attempt is not even a name.
    assert!(remove_skill(home.path(), "../pack").is_err());
    assert!(remove_skill(home.path(), "").is_err());

    assert_eq!(
        remove_skill(home.path(), "mine").expect("it deletes"),
        "Deleted skill 'mine'"
    );
    assert!(!user.join("mine").exists(), "the directory should be gone");
    // Deleting twice says so instead of pretending.
    assert!(remove_skill(home.path(), "mine").is_err());
}

#[test]
fn the_form_fence_says_what_is_wrong_with_the_input() {
    let base = ServerSpec {
        name: "fs".to_string(),
        ..ServerSpec::default()
    };
    // Neither transport: the one mistake a form makes that a file rarely does.
    assert!(validate_server(&base).unwrap_err().contains("set either"));
    // Both transports: same refusal the runtime gives, in form language.
    let both = ServerSpec {
        url: Some("https://h/mcp".to_string()),
        command: Some("npx".to_string()),
        ..base.clone()
    };
    assert!(both_url_err(&both).contains("not both"));
    // A url that is not http(s), a name with spaces, an auth_env that is not a name.
    let bad_url = ServerSpec {
        url: Some("ftp://h".to_string()),
        ..base.clone()
    };
    assert!(validate_server(&bad_url).unwrap_err().contains("http"));
    let bad_name = ServerSpec {
        name: "my server".to_string(),
        ..base.clone()
    };
    assert!(validate_server(&bad_name).unwrap_err().contains("prefix"));
    let bad_key = ServerSpec {
        url: Some("https://h/mcp".to_string()),
        auth_env: Some("my-token".to_string()),
        ..base.clone()
    };
    assert!(validate_server(&bad_key).unwrap_err().contains("env var"));

    // The documented shapes pass.
    let stdio = ServerSpec {
        command: Some("npx".to_string()),
        args: Some(vec!["-y".to_string(), "server".to_string()]),
        ..base.clone()
    };
    validate_server(&stdio).expect("stdio is legal");
    let http = ServerSpec {
        url: Some("https://h/mcp".to_string()),
        auth_env: Some("NOTES_API_KEY".to_string()),
        ..base
    };
    validate_server(&http).expect("http is legal");
}

fn both_url_err(spec: &ServerSpec) -> String {
    validate_server(spec).unwrap_err()
}

#[test]
fn mcp_rows_merge_like_the_runtime_does() {
    let home = tempfile::tempdir().expect("temp dir");
    let user = home.path().join("mcp.json");
    let pack = home.path().join("pack.json");
    std::fs::write(
        &user,
        json!({"servers": [
            {"name": "fs", "command": "npx", "args": ["-y", "server"]},
            {"name": "notes", "url": "https://h/mcp", "auth_env": "NOTES_API_KEY"}
        ]})
        .to_string(),
    )
    .expect("write the user config");
    std::fs::write(
        &pack,
        json!({"servers": [
            {"name": "notes", "url": "https://other/mcp"},
            {"name": "cloud", "url": "https://h/mcp", "oauth": true},
            {"name": "", "command": "x"}
        ]})
        .to_string(),
    )
    .expect("write the built-in pack");

    let (rows, error) = mcp_rows_in(&[user, pack]);
    assert!(
        error.is_none(),
        "the user config should be readable: {error:?}"
    );
    let by_name = |name: &str| {
        rows.iter()
            .find(|row| row.name == name)
            .expect("this row should be here")
    };

    assert_eq!(
        rows.len(),
        3,
        "nameless entries are skipped and duplicates not repeated: {rows:?}"
    );
    assert_eq!(by_name("fs").source, "user");
    assert_eq!(by_name("fs").kind, "command");
    assert_eq!(by_name("fs").args, vec!["-y", "server"]);
    assert_eq!(by_name("fs").auth, "none");
    assert_eq!(
        by_name("notes").source,
        "user",
        "the first one read wins — the same as at runtime"
    );
    assert_eq!(by_name("notes").auth, "auth_env");
    assert_eq!(by_name("notes").auth_env.as_deref(), Some("NOTES_API_KEY"));
    assert_eq!(by_name("cloud").source, "builtin");
    assert_eq!(by_name("cloud").auth, "oauth");
}

#[test]
fn a_broken_user_config_is_reported_a_broken_pack_is_not_fatal() {
    let home = tempfile::tempdir().expect("temp dir");
    let user = home.path().join("mcp.json");
    let pack = home.path().join("pack.json");
    std::fs::write(&user, "{ not json").expect("write the broken user config");
    std::fs::write(&pack, "{ also not json").expect("write the broken built-in pack");

    let (rows, error) = mcp_rows_in(&[user, pack]);
    assert!(rows.is_empty());
    let error = error.expect("an unreadable user file has to be reported");
    assert!(error.contains("mcp.json"), "{error}");
}

#[test]
fn saving_a_server_refuses_a_name_the_builtin_pack_owns() {
    let home = tempfile::tempdir().expect("temp dir");
    let pack = home.path().join("pack.json");
    std::fs::write(
        &pack,
        json!({"servers": [{"name": "cloud", "url": "https://h/mcp"}]}).to_string(),
    )
    .expect("write the built-in pack");

    let spec = ServerSpec {
        name: "cloud".to_string(),
        url: Some("https://mine/mcp".to_string()),
        ..ServerSpec::default()
    };
    // Shadowing would be worse than refusing: the runtime's first-wins would make the
    // user's edit do nothing at all.
    let refusal = save_server_among(home.path(), &spec, &[pack])
        .expect_err("a name a built-in owns is refused");
    assert!(refusal.contains("built-in"), "{refusal}");
    assert!(
        !home.path().join("mcp.json").exists(),
        "a refusal must not touch the disk"
    );
}

#[test]
fn saving_a_server_creates_upserts_and_never_duplicates() {
    let home = tempfile::tempdir().expect("temp dir");
    let fs = ServerSpec {
        name: "fs".to_string(),
        command: Some("npx".to_string()),
        args: Some(vec!["-y".to_string(), "server".to_string()]),
        ..ServerSpec::default()
    };
    let notes = ServerSpec {
        name: "notes".to_string(),
        url: Some("https://h/mcp".to_string()),
        auth_env: Some("NOTES_API_KEY".to_string()),
        ..ServerSpec::default()
    };

    // No file yet: the first save writes one.
    save_server_among(home.path(), &fs, &[]).expect("the first save");
    save_server_among(home.path(), &notes, &[]).expect("the second entry");
    let text = std::fs::read_to_string(home.path().join("mcp.json")).expect("read the config");
    assert_eq!(
        text.matches("\"name\"").count(),
        2,
        "one line per entry: {text}"
    );
    assert!(
        text.contains("\"args\""),
        "the arguments have to be written: {text}"
    );
    assert!(
        !text.contains("null"),
        "an unset field must not be written as null: {text}"
    );

    // Replace by name: still two rows, the new values in place.
    let moved = ServerSpec {
        url: Some("https://moved/mcp".to_string()),
        ..notes.clone()
    };
    save_server_among(home.path(), &moved, &[]).expect("save under the new name");
    let text = std::fs::read_to_string(home.path().join("mcp.json")).expect("read it again");
    assert_eq!(
        text.matches("\"name\"").count(),
        2,
        "the same name replaces rather than adds: {text}"
    );
    assert!(text.contains("https://moved/mcp"), "{text}");

    // And the round trip reads back as rows.
    let (rows, error) = mcp_rows_in(&[home.path().join("mcp.json")]);
    assert!(error.is_none());
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].name, "fs");
    assert_eq!(rows[1].url.as_deref(), Some("https://moved/mcp"));
}

#[test]
fn removing_a_server_only_touches_the_users_file() {
    let home = tempfile::tempdir().expect("temp dir");
    let path: PathBuf = home.path().join("mcp.json");
    std::fs::write(
        &path,
        json!({"servers": [
            {"name": "fs", "command": "npx"},
            {"name": "notes", "url": "https://h/mcp"}
        ]})
        .to_string(),
    )
    .expect("write the config");

    assert!(remove_server(home.path(), "cloud")
        .unwrap_err()
        .contains("no connector named"));
    assert!(remove_server(home.path(), "../escape").is_err());
    remove_server(home.path(), "fs").expect("it deletes");

    let text = std::fs::read_to_string(&path).expect("read the config");
    assert!(!text.contains("\"fs\""), "it should be gone: {text}");
    assert!(
        text.contains("\"notes\""),
        "the other one must be left alone: {text}"
    );
}

#[test]
fn removing_from_a_missing_file_says_so() {
    let home = tempfile::tempdir().expect("temp dir");
    let refusal = remove_server(home.path(), "fs").expect_err("there is no file");
    assert!(refusal.contains("there is no"), "{refusal}");
}
