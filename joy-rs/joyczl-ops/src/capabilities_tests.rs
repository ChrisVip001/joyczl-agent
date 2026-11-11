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
    std::fs::create_dir_all(&folder).expect("建目录");
    std::fs::write(
        folder.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\n正文\n"),
    )
    .expect("写技能");
}

#[test]
fn skills_carry_where_they_came_from_and_home_wins() {
    let home = tempfile::tempdir().expect("临时目录");
    let user = home.path().join("skills");
    let pack = home.path().join("pack");
    std::fs::create_dir_all(&user).expect("建用户目录");
    std::fs::create_dir_all(&pack).expect("建内置包");

    put_skill(&user, "mine", "我自己写的");
    put_skill(&pack, "bundled", "随 Joy 分发的");
    // Same name in both: the user's copy wins — and the row says so.
    put_skill(&pack, "shared", "内置包里的版本");
    put_skill(&user, "shared", "我自己改过的版本");
    // The staging area is not a skill, and a SKILL.md in the root is not either.
    std::fs::create_dir_all(pack.join("_incoming")).expect("建暂存");
    std::fs::write(
        pack.join("_incoming/SKILL.md"),
        "---\nname: x\ndescription: y\n---\n",
    )
    .expect("写");
    std::fs::write(
        pack.join("SKILL.md"),
        "---\nname: root\ndescription: y\n---\n",
    )
    .expect("写");

    let (rows, error) = skill_rows_in(&[user.clone(), pack.clone()]);
    assert!(error.is_none(), "没有错误该报：{error:?}");
    let by_name = |name: &str| {
        rows.iter()
            .find(|row| row.name == name)
            .expect("该有这一行")
    };

    assert_eq!(by_name("mine").source, "user");
    assert_eq!(by_name("bundled").source, "builtin");
    // The shared one is the user's row — same priority the loader uses.
    assert_eq!(by_name("shared").source, "user");
    assert_eq!(by_name("shared").description, "我自己改过的版本");
    assert_eq!(rows.len(), 3, "暂存区和根目录里的不算：{rows:?}");
}

#[test]
fn a_missing_user_directory_is_not_an_error() {
    let home = tempfile::tempdir().expect("临时目录");
    let (rows, error) = skill_rows_in(&[home.path().join("skills"), home.path().join("pack")]);
    assert!(rows.is_empty());
    assert!(
        error.is_none(),
        "没建过技能目录是新装机的默认状态，不是错误"
    );
}

#[test]
fn only_user_skills_can_be_deleted() {
    let home = tempfile::tempdir().expect("临时目录");
    let user = home.path().join("skills");
    let pack = home.path().join("pack");
    std::fs::create_dir_all(&user).expect("建目录");
    put_skill(&user, "mine", "我的");
    put_skill(&pack, "bundled", "内置的");

    // The built-in one does not live under home/skills — refused, not "not found".
    let refusal = remove_skill(home.path(), "bundled").expect_err("内置技能删不得");
    assert!(refusal.contains("内置"), "{refusal}");

    // A traversal attempt is not even a name.
    assert!(remove_skill(home.path(), "../pack").is_err());
    assert!(remove_skill(home.path(), "").is_err());

    assert_eq!(
        remove_skill(home.path(), "mine").expect("删得掉"),
        "已删除技能 'mine'"
    );
    assert!(!user.join("mine").exists(), "目录该没了");
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
    assert!(validate_server(&base).unwrap_err().contains("之一"));
    // Both transports: same refusal the runtime gives, in form language.
    let both = ServerSpec {
        url: Some("https://h/mcp".to_string()),
        command: Some("npx".to_string()),
        ..base.clone()
    };
    assert!(both_url_err(&both).contains("只能填一个"));
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
    assert!(validate_server(&bad_name).unwrap_err().contains("前缀"));
    let bad_key = ServerSpec {
        url: Some("https://h/mcp".to_string()),
        auth_env: Some("my-token".to_string()),
        ..base.clone()
    };
    assert!(validate_server(&bad_key).unwrap_err().contains("环境变量"));

    // The documented shapes pass.
    let stdio = ServerSpec {
        command: Some("npx".to_string()),
        args: Some(vec!["-y".to_string(), "server".to_string()]),
        ..base.clone()
    };
    validate_server(&stdio).expect("stdio 合法");
    let http = ServerSpec {
        url: Some("https://h/mcp".to_string()),
        auth_env: Some("NOTES_API_KEY".to_string()),
        ..base
    };
    validate_server(&http).expect("http 合法");
}

fn both_url_err(spec: &ServerSpec) -> String {
    validate_server(spec).unwrap_err()
}

#[test]
fn mcp_rows_merge_like_the_runtime_does() {
    let home = tempfile::tempdir().expect("临时目录");
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
    .expect("写用户配置");
    std::fs::write(
        &pack,
        json!({"servers": [
            {"name": "notes", "url": "https://other/mcp"},
            {"name": "cloud", "url": "https://h/mcp", "oauth": true},
            {"name": "", "command": "x"}
        ]})
        .to_string(),
    )
    .expect("写内置包");

    let (rows, error) = mcp_rows_in(&[user, pack]);
    assert!(error.is_none(), "用户配置读得动：{error:?}");
    let by_name = |name: &str| {
        rows.iter()
            .find(|row| row.name == name)
            .expect("该有这一行")
    };

    assert_eq!(rows.len(), 3, "无名条目跳过，重名的不重复：{rows:?}");
    assert_eq!(by_name("fs").source, "user");
    assert_eq!(by_name("fs").kind, "command");
    assert_eq!(by_name("fs").args, vec!["-y", "server"]);
    assert_eq!(by_name("fs").auth, "none");
    assert_eq!(
        by_name("notes").source,
        "user",
        "先读到的赢 —— 和运行时一样"
    );
    assert_eq!(by_name("notes").auth, "auth_env");
    assert_eq!(by_name("notes").auth_env.as_deref(), Some("NOTES_API_KEY"));
    assert_eq!(by_name("cloud").source, "builtin");
    assert_eq!(by_name("cloud").auth, "oauth");
}

#[test]
fn a_broken_user_config_is_reported_a_broken_pack_is_not_fatal() {
    let home = tempfile::tempdir().expect("临时目录");
    let user = home.path().join("mcp.json");
    let pack = home.path().join("pack.json");
    std::fs::write(&user, "{ not json").expect("写坏用户配置");
    std::fs::write(&pack, "{ also not json").expect("写坏内置包");

    let (rows, error) = mcp_rows_in(&[user, pack]);
    assert!(rows.is_empty());
    let error = error.expect("用户那份读不动必须说");
    assert!(error.contains("mcp.json"), "{error}");
}

#[test]
fn saving_a_server_refuses_a_name_the_builtin_pack_owns() {
    let home = tempfile::tempdir().expect("临时目录");
    let pack = home.path().join("pack.json");
    std::fs::write(
        &pack,
        json!({"servers": [{"name": "cloud", "url": "https://h/mcp"}]}).to_string(),
    )
    .expect("写内置包");

    let spec = ServerSpec {
        name: "cloud".to_string(),
        url: Some("https://mine/mcp".to_string()),
        ..ServerSpec::default()
    };
    // Shadowing would be worse than refusing: the runtime's first-wins would make the
    // user's edit do nothing at all.
    let refusal = save_server_among(home.path(), &spec, &[pack]).expect_err("内置占用的名字拒写");
    assert!(refusal.contains("系统内置"), "{refusal}");
    assert!(!home.path().join("mcp.json").exists(), "被拒就不该落盘");
}

#[test]
fn saving_a_server_creates_upserts_and_never_duplicates() {
    let home = tempfile::tempdir().expect("临时目录");
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
    save_server_among(home.path(), &fs, &[]).expect("首次保存");
    save_server_among(home.path(), &notes, &[]).expect("第二条");
    let text = std::fs::read_to_string(home.path().join("mcp.json")).expect("读配置");
    assert_eq!(text.matches("\"name\"").count(), 2, "两条各一行：{text}");
    assert!(text.contains("\"args\""), "参数要写进去：{text}");
    assert!(!text.contains("null"), "没填的字段不该写成 null：{text}");

    // Replace by name: still two rows, the new values in place.
    let moved = ServerSpec {
        url: Some("https://moved/mcp".to_string()),
        ..notes.clone()
    };
    save_server_among(home.path(), &moved, &[]).expect("改名保存");
    let text = std::fs::read_to_string(home.path().join("mcp.json")).expect("再读");
    assert_eq!(
        text.matches("\"name\"").count(),
        2,
        "同名是替换不是新增：{text}"
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
    let home = tempfile::tempdir().expect("临时目录");
    let path: PathBuf = home.path().join("mcp.json");
    std::fs::write(
        &path,
        json!({"servers": [
            {"name": "fs", "command": "npx"},
            {"name": "notes", "url": "https://h/mcp"}
        ]})
        .to_string(),
    )
    .expect("写配置");

    assert!(remove_server(home.path(), "cloud")
        .unwrap_err()
        .contains("没有叫"));
    assert!(remove_server(home.path(), "../escape").is_err());
    remove_server(home.path(), "fs").expect("删得掉");

    let text = std::fs::read_to_string(&path).expect("读配置");
    assert!(!text.contains("\"fs\""), "该没了：{text}");
    assert!(text.contains("\"notes\""), "别的不该被动：{text}");
}

#[test]
fn removing_from_a_missing_file_says_so() {
    let home = tempfile::tempdir().expect("临时目录");
    let refusal = remove_server(home.path(), "fs").expect_err("没有文件");
    assert!(refusal.contains("还没有"), "{refusal}");
}
