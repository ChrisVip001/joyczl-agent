//! 只测一件事：**哪些通知属于这一轮**。
//!
//! 广播是给所有订阅者的，app-server 又是一个进程跑所有人的会话 ——
//! 网关那边另一个会话跑起来的时候，它的 `textDelta` 也会流到驾驶舱这条
//! 连接上。过滤错了的表现是「回复里混进别人的字」，所以这里把它钉住。

use serde_json::json;

use super::belongs;
use joyczl_protocol::ServerNotification;

fn note(value: serde_json::Value) -> ServerNotification {
    serde_json::from_value(value).expect("测试用的通知")
}

fn started(session: &str, turn: &str) -> ServerNotification {
    note(json!({
        "type": "turnStarted",
        "turnId": turn,
        "sessionId": session,
        "userMessage": "你好",
        "ts": "2026-01-01T00:00:00+08:00",
    }))
}

fn delta(turn: &str) -> ServerNotification {
    note(json!({"type": "textDelta", "turnId": turn, "delta": "字"}))
}

#[test]
fn 第一帧靠会话认_之后的帧靠轮次认() {
    let opened = started("s1", "t1");
    assert_eq!(belongs(&opened, "s1", None).as_deref(), Some("t1"));

    // 认下这一轮之后，同 id 的都放行。
    assert_eq!(
        belongs(&delta("t1"), "s1", Some("t1")).as_deref(),
        Some("t1")
    );
    // 另一个轮次的丢掉 —— 即使会话 id 一样（并发跑两轮时会出现）。
    assert_eq!(belongs(&delta("t2"), "s1", Some("t1")), None);
}

#[test]
fn 别的会话的通知不会混进来() {
    assert_eq!(belongs(&started("s2", "t9"), "s1", None), None);
    // 还没认下自己那一轮之前，什么都不转发。
    assert_eq!(belongs(&delta("t9"), "s1", None), None);
}

#[test]
fn 没有轮次id的通知_认下之后照常转发() {
    // consolidationCompleted 和 graph* 不带 turnId：它们跟会话同属一轮。
    let consolidation = note(json!({"type": "consolidationCompleted", "newFacts": 2}));
    assert_eq!(
        belongs(&consolidation, "s1", Some("t1")).as_deref(),
        Some("t1")
    );
    // 但没认下之前，它们同样无处可去。
    assert_eq!(belongs(&consolidation, "s1", None), None);
}

/// 扩展名白名单：只放行 1-8 位字母数字；双扩展只看最后一段；路径形状、
/// 汉字、超长一律不放 —— 白名单之外的都落 `bin`（不报错，文件照样能传）。
#[test]
fn upload_extension_whitelist_is_strict() {
    use super::safe_extension;

    assert_eq!(safe_extension("a.txt").as_deref(), Some("txt"));
    assert_eq!(safe_extension("报告.PDF").as_deref(), Some("pdf"));
    assert_eq!(safe_extension("no_ext").as_deref(), None, "无扩展名");
    assert_eq!(
        safe_extension("x.tar.gz").as_deref(),
        Some("gz"),
        "只看最后一段"
    );
    assert_eq!(
        safe_extension("../../etc/passwd").as_deref(),
        None,
        "路径形状进不来"
    );
    assert_eq!(safe_extension("a.带汉字").as_deref(), None);
    assert_eq!(safe_extension("a.0123456789").as_deref(), None, "超过 8 位");
}

/// 目录浏览：只列子目录、按名排序、藏掉点开头与构建噪音；相对路径与
/// 不存在的路径给出能照着做的话。
#[test]
fn fs_browsing_lists_only_directories_and_hides_noise() {
    use super::list_dirs;

    let dir = tempfile::tempdir().expect("临时目录");
    let root = dir.path();
    std::fs::create_dir(root.join("项目甲")).unwrap();
    std::fs::create_dir(root.join("beta")).unwrap();
    std::fs::write(root.join("file.txt"), "不是目录").unwrap();
    std::fs::create_dir(root.join(".hidden")).unwrap();
    std::fs::create_dir(root.join("target")).unwrap();

    let listing = list_dirs(&root.to_string_lossy(), false).expect("列得动");
    let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["beta", "项目甲"],
        "只有子目录、排序、没有点开头与 target"
    );
    assert_eq!(
        listing.parent.as_deref(),
        Some(root.parent().unwrap().to_string_lossy().as_ref())
    );

    assert!(list_dirs("相对/路径", false).is_err(), "相对路径拒绝");
    assert!(list_dirs("/不存在/的/目录/joy", false).is_err());
}

/// 隐藏目录开关（dsh 的 showHidden）：默认藏点开头，开了才露；
/// 构建噪音无论如何都不露。
#[test]
fn hidden_directories_show_only_when_asked() {
    use super::list_dirs;

    let dir = tempfile::tempdir().expect("临时目录");
    let root = dir.path();
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::create_dir(root.join("node_modules")).unwrap();
    std::fs::create_dir(root.join("normal")).unwrap();

    let hidden_off = list_dirs(&root.to_string_lossy(), false).expect("列得动");
    let names: Vec<&str> = hidden_off.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["normal"], "默认只有普通目录");

    let hidden_on = list_dirs(&root.to_string_lossy(), true).expect("列得动");
    let names: Vec<&str> = hidden_on.entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&".git"), "开了开关能看到点开头");
    assert!(!names.contains(&"node_modules"), "构建噪音永远不露");
}

/// 新建文件夹（dsh 的 createDirectory）：合格名字建得出来；路径形状
/// （分隔符、..、点开头）、超长、重名、坏父目录都挡下 —— 用户输入
/// 进不了路径形状，与 upload 的扩展名白名单同一条纪律。
#[test]
fn folder_creation_rejects_path_shapes() {
    use super::create_subdir;

    let dir = tempfile::tempdir().expect("临时目录");
    let root = dir.path().to_string_lossy().to_string();

    let created = create_subdir(&root, "新项目").expect("建得出来");
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
        assert!(create_subdir(&root, bad).is_err(), "该挡：{bad:?}");
    }
    assert!(create_subdir(&root, "新项目").is_err(), "重名挡下");
    assert!(create_subdir("/不存在/joy", "x").is_err(), "坏父目录挡下");
}
