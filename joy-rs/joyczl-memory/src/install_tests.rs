//! 技能安装/更新的测试：版本比对、校验先于写入、备份、原子替换。

use super::install::{
    install, installed_version, needs_update, parse_index, update_all, IndexEntry, Outcome,
};

const GOOD: &str = "---\nname: weekly-review\ndescription: summarize the week\nversion: 1.1.0\n---\n1. pull episodes\n";

fn entry(name: &str, version: &str, url: &str) -> IndexEntry {
    IndexEntry {
        name: name.to_string(),
        version: version.to_string(),
        url: url.to_string(),
    }
}

#[test]
fn a_bad_entry_is_skipped_not_fatal() {
    let (entries, warnings) = parse_index(
        r#"{"skills":[
            {"name":"a","version":"1.0.0","url":"https://x/a.md"},
            {"name":"b","url":"https://x/b.md"},
            {"name":"c","version":"1.0.0"}
        ]}"#,
    )
    .expect("解析");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "a");
    assert_eq!(warnings.len(), 2, "坏条目要逐条说明：{warnings:?}");

    assert!(parse_index("这不是 JSON").is_err());
    assert!(parse_index(r#"{"nope":[]}"#).is_err());
}

#[test]
fn version_comparison_never_downgrades() {
    assert!(needs_update(None, "1.0.0"), "没装过就装");
    assert!(needs_update(Some("1.0.0"), "1.1.0"));
    assert!(needs_update(Some("1.9.0"), "1.10.0"), "按段比大小");
    assert!(!needs_update(Some("1.1.0"), "1.1.0"));
    assert!(!needs_update(Some("2.0.0"), "1.9.0"), "不降级");
    // 比不了大小的（非数字）就老实不动。
    assert!(!needs_update(Some("2024-05"), "2024-06"));
}

#[test]
fn install_validates_before_touching_the_disk() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path();

    // 名字不对版。
    let wrong = install(
        home,
        "weekly-review",
        "---\nname: other\ndescription: x\n---\nb",
    );
    assert!(wrong.is_err(), "名不对版要拒绝");
    // 内容不合格。
    assert!(install(home, "weekly-review", "没有 frontmatter").is_err());
    // 路径穿越。
    assert!(install(home, "../evil", GOOD).is_err());
    assert!(!home.join("skills").join("evil").exists());
    // 一路拒绝下来，skills 目录里什么都不该有。
    assert!(installed_version(home, "weekly-review").is_none());

    // 合格的才落盘。
    let path = install(home, "weekly-review", GOOD).expect("装上");
    assert!(path.exists());
    assert_eq!(
        installed_version(home, "weekly-review").as_deref(),
        Some("1.1.0")
    );
}

#[test]
fn a_reinstall_backs_up_the_previous_version() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path();
    install(home, "weekly-review", GOOD).expect("第一版");

    let second = GOOD
        .replace("version: 1.1.0", "version: 1.2.0")
        .replace("pull episodes", "pull episodes and summarize");
    install(home, "weekly-review", &second).expect("第二版");

    assert_eq!(
        installed_version(home, "weekly-review").as_deref(),
        Some("1.2.0")
    );
    // 旧版躺在 .backup 里，可回退。
    let backups = std::fs::read_dir(home.join("skills").join(".backup"))
        .expect("备份目录")
        .count();
    assert_eq!(backups, 1, "替换前要备份");
    // 暂存目录不留垃圾。
    let staged = std::fs::read_dir(home.join("skills").join(".staging"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(staged, 0, "暂存目录该被搬空");
}

#[tokio::test]
async fn update_all_installs_skips_and_reports_failures() {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path();
    // 已装一个 1.1.0，索引里有一个同版本、一个更新、一个坏内容、一个取不回。
    install(home, "weekly-review", GOOD).expect("先装一版");

    let entries = vec![
        entry("weekly-review", "1.1.0", "same"),
        entry("roadmap", "2.0.0", "good"),
        entry("broken", "1.0.0", "bad-content"),
        entry("missing", "1.0.0", "unreachable"),
    ];
    let outcomes = update_all(home, &entries, |url: String| async move {
        match url.as_str() {
            "same" => Ok(GOOD.to_string()),
            "good" => Ok(
                "---\nname: roadmap\ndescription: plan quarters\nversion: 2.0.0\n---\nsteps"
                    .to_string(),
            ),
            "bad-content" => Ok("这不是 SKILL.md".to_string()),
            _ => Err("404".to_string()),
        }
    })
    .await;

    assert!(
        matches!(outcomes[0], Outcome::UpToDate { .. }),
        "{:?}",
        outcomes[0]
    );
    assert!(matches!(outcomes[1], Outcome::Installed { .. }));
    assert!(
        matches!(outcomes[2], Outcome::Failed { .. }),
        "坏内容要报告"
    );
    assert!(
        matches!(outcomes[3], Outcome::Failed { .. }),
        "取不回要报告"
    );

    // 报告失败的没有落盘。
    assert!(installed_version(home, "broken").is_none());
    // 成功的落盘了。
    assert_eq!(installed_version(home, "roadmap").as_deref(), Some("2.0.0"));
    // ok 的只有两条（已是最新 + 装上），两条失败的不算。
    assert_eq!(outcomes.iter().filter(|o| o.ok()).count(), 2);
    assert!(
        outcomes.iter().all(|o| !o.line().is_empty()),
        "每条都要有一句能打印给人看的话"
    );
}
