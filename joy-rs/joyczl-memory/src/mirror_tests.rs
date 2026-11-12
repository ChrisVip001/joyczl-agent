//! Mirror tests: the two promises — one page per subject, and manual edits survive.

use joyczl_state::Facts;

use crate::bundle::Bundle;
use crate::mirror::mirror_facts;

async fn fixtures(name: &str) -> (Facts, Bundle) {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().join(name);
    std::fs::create_dir_all(&home).expect("建目录");
    let _ = dir.keep();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    (Facts::new(pool), Bundle::new(&home))
}

/// 一个 subject 一页：正文是事实列表，出处指向 fact id，索引同步。
#[tokio::test]
async fn a_subject_becomes_one_page() {
    let (facts, bundle) = fixtures("mirror_subject").await;
    let (first, _) = facts
        .add("alex", "喜欢早会", "user", "user")
        .await
        .expect("写");
    let _ = facts
        .add("alex", "住在杭州", "user", "user")
        .await
        .expect("写");

    let report = mirror_facts(&facts, &bundle).await.expect("镜像");
    assert_eq!(report.written, 1);
    assert_eq!(report.skipped, 0);

    let page = bundle.read("alex").expect("读").expect("有页");
    assert_eq!(page.title, "alex");
    assert!(page.body.contains("喜欢早会"), "{}", page.body);
    assert!(page.body.contains("住在杭州"), "{}", page.body);
    assert!(
        page.sources
            .iter()
            .any(|source| source == &format!("fact:{first}", first = first.id)),
        "出处要指回事实行"
    );
    assert!(bundle.index_text().expect("目录").contains("alex.md"));

    // 索引侧也记下了：这一页是哪一页、什么指纹。
    let meta = facts.meta(first.id).await.expect("查").expect("有");
    assert_eq!(meta.page_path.as_deref(), Some("memory/alex.md"));
    assert_eq!(meta.digest.as_deref(), Some(page.digest.as_str()));
}

/// 人手改过的页不覆盖：镜像跳过它，并说清楚原因。
#[tokio::test]
async fn a_hand_edited_page_is_not_overwritten() {
    let (facts, bundle) = fixtures("mirror_handedit").await;
    facts
        .add("alex", "喜欢早会", "user", "user")
        .await
        .expect("写");
    mirror_facts(&facts, &bundle).await.expect("第一次镜像");

    // 人改一句。
    let path = bundle.root().join("alex.md");
    let edited = std::fs::read_to_string(&path)
        .expect("读")
        .replace("喜欢早会", "改成了晚饭后");
    std::fs::write(&path, &edited).expect("手改");

    // 又有一条新事实进来。
    facts
        .add("alex", "养了一只猫", "user", "user")
        .await
        .expect("写");
    let report = mirror_facts(&facts, &bundle).await.expect("第二次镜像");
    assert_eq!(report.written, 0, "不该覆盖");
    assert_eq!(report.skipped, 1, "该记一笔跳过");

    let page = bundle.read("alex").expect("读").expect("有页");
    assert!(page.body.contains("改成了晚饭后"), "人的改动要留下");
    assert!(!page.body.contains("养了一只猫"), "新事实这次没进文件");
}

/// 没有事实时不写空页；索引也不该凭空多一行。
#[tokio::test]
async fn an_empty_store_writes_nothing() {
    let (facts, bundle) = fixtures("mirror_empty").await;
    let report = mirror_facts(&facts, &bundle).await.expect("镜像");
    assert_eq!(report.written, 0);
    assert!(bundle.pages().expect("列").is_empty());
}

/// 中文 subject 也要有稳定的页面名（ASCII 映射后为空时退到哈希）。
#[tokio::test]
async fn a_chinese_subject_gets_a_stable_slug() {
    let (facts, bundle) = fixtures("mirror_cjk").await;
    facts
        .add("张伟", "在上海工作", "user", "user")
        .await
        .expect("写");
    mirror_facts(&facts, &bundle).await.expect("镜像");

    let pages = bundle.pages().expect("列");
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].slug, "张伟", "中文主题就用中文页名");

    // 再跑一次还是同一页：名字不能每次变。
    let again = mirror_facts(&facts, &bundle).await.expect("再镜像");
    assert_eq!(again.written, 1);
    assert_eq!(bundle.pages().expect("列").len(), 1, "不该长出第二页");
}
