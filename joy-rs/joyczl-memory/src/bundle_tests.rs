//! Bundle tests: the claim is "plain files you can trust", so every test is about what
//! survives an edit, a crash, or a hostile slug.

use crate::bundle::{Bundle, Page};

fn temp_bundle(name: &str) -> Bundle {
    let dir = tempfile::tempdir().expect("临时目录");
    let root = dir.path().join(name);
    let _ = dir.keep();
    let bundle = Bundle::from_root(&root);
    bundle.ensure().expect("建库");
    bundle
}

/// 写一页、读回来：正文与头部字段都还在。
#[test]
fn a_page_round_trips() {
    let bundle = temp_bundle("page_roundtrip");
    let mut page = Page::new(
        "alex",
        "person",
        "Alex",
        "alex",
        "喜欢早会",
        "第一行\n第二行",
    );
    page.sources = vec!["ledger:s1/t1".to_string()];
    bundle.write(&page).expect("写页");

    let read = bundle.read("alex").expect("读").expect("存在");
    assert_eq!(read.kind, "person");
    assert_eq!(read.title, "Alex");
    assert_eq!(read.summary, "喜欢早会");
    assert_eq!(read.body, "第一行\n第二行");
    assert_eq!(read.sources, vec!["ledger:s1/t1".to_string()]);
    assert_eq!(read.digest, page.digest);
}

/// 目录文件跟着页面走：一页一行，带标题、类型与一句话说明。
#[test]
fn the_catalog_is_regenerated_from_the_pages() {
    let bundle = temp_bundle("page_index");
    bundle
        .write(&Page::new(
            "alex",
            "person",
            "Alex",
            "alex",
            "喜欢早会",
            "正文",
        ))
        .expect("写");
    bundle
        .write(&Page::new(
            "joy",
            "project",
            "Joy",
            "joy",
            "本地优先助手",
            "正文",
        ))
        .expect("写");

    let index = bundle.index_text().expect("目录");
    assert!(index.contains("2 pages"), "计数要对：{index}");
    assert!(
        index.contains("[Alex](alex.md) · person — 喜欢早会"),
        "{index}"
    );
    assert!(
        index.contains("[Joy](joy.md) · project — 本地优先助手"),
        "{index}"
    );
}

/// 手改正文后，读出来的 digest 跟着正文变 —— 「文件说了算」不是口号。
#[test]
fn the_digest_follows_a_hand_edit() {
    let bundle = temp_bundle("page_handedit");
    bundle
        .write(&Page::new("alex", "fact", "Alex", "alex", "摘要", "原文"))
        .expect("写");

    let path = bundle.root().join("alex.md");
    let edited = std::fs::read_to_string(&path)
        .expect("读")
        .replace("原文", "被人改过");
    std::fs::write(&path, edited).expect("手改");

    let read = bundle.read("alex").expect("读").expect("存在");
    assert_eq!(read.body, "被人改过");
    assert_ne!(
        read.digest,
        crate::bundle::digest_of("原文"),
        "摘要必须反映正文"
    );
    assert_eq!(read.digest, crate::bundle::digest_of("被人改过"));
}

/// 删除一页：文件没了，目录少一行，变更历史里留痕。
#[test]
fn removing_a_page_leaves_a_trail() {
    let bundle = temp_bundle("page_remove");
    bundle
        .write(&Page::new("gone", "fact", "Gone", "gone", "", "正文"))
        .expect("写");
    assert!(bundle.remove("gone").expect("删"));
    assert!(!bundle.remove("gone").expect("再删"), "已经不在了");

    assert!(bundle.read("gone").expect("读").is_none());
    assert!(!bundle.index_text().expect("目录").contains("gone.md"));
    let trail = std::fs::read_to_string(bundle.root().join("changelog.md")).expect("历史");
    assert!(trail.contains("write gone fact"), "{trail}");
    assert!(trail.contains("remove gone"), "{trail}");
}

/// 坏页面不连坐：一个手写坏的文件只是被跳过，其他页照旧可用。
#[test]
fn one_broken_page_does_not_hide_the_rest() {
    let bundle = temp_bundle("page_broken");
    bundle
        .write(&Page::new("good", "fact", "Good", "good", "还在", "正文"))
        .expect("写");
    std::fs::write(bundle.root().join("broken.md"), "没有头部的一段话\n").expect("写坏文件");

    let pages = bundle.pages().expect("列");
    assert_eq!(pages.len(), 1, "坏的跳过，好的还在");
    assert_eq!(pages[0].slug, "good");
}

/// 页面名是文件名：越界、保留名、大写都被拒绝，而且拒绝得清楚。
#[test]
fn slug_validation_refuses_traversal() {
    let bundle = temp_bundle("page_slug");
    for bad in [
        "../etc/passwd",
        "a/b",
        "Index",
        "",
        "index",
        "changelog",
        "带 空格",
    ] {
        let error = bundle
            .page_path(bad)
            .err()
            .unwrap_or_else(|| panic!("`{bad}` 该被拒绝"));
        assert!(!error.to_string().is_empty(), "拒绝要带原因");
    }
    assert!(
        bundle.page_path("weekly-review_2").is_ok(),
        "正常名字要放行"
    );
    assert!(
        bundle.page_path("带中文").is_ok(),
        "中文页名要放行 —— 人手写的库该是可读的"
    );
}

/// 页面的头部字段是 OKF 的形状：`type` 在第一行，正文在 `---` 之后。
#[test]
fn a_page_file_starts_with_its_type() {
    let bundle = temp_bundle("page_shape");
    bundle
        .write(&Page::new("alex", "person", "Alex", "alex", "摘要", "正文"))
        .expect("写");
    let text = std::fs::read_to_string(bundle.root().join("alex.md")).expect("读");
    assert!(text.starts_with("---\ntype: person\n"), "{text}");
    assert!(text.contains("title: Alex\n"), "{text}");
    assert!(text.trim_end().ends_with("正文"), "{text}");
}
