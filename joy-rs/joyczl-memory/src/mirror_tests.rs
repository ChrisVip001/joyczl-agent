//! Mirror tests: the two promises — one page per subject, and manual edits survive.

use joyczl_state::Facts;

use crate::bundle::Bundle;
use crate::mirror::mirror_facts;

async fn fixtures(name: &str) -> (Facts, Bundle) {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = dir.path().join(name);
    std::fs::create_dir_all(&home).expect("create the directory");
    let _ = dir.keep();
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("open the store");
    (Facts::new(pool), Bundle::new(&home))
}

/// One page per subject: the body is the list of facts, the source points at a fact id, and
/// the index keeps up.
#[tokio::test]
async fn a_subject_becomes_one_page() {
    let (facts, bundle) = fixtures("mirror_subject").await;
    let (first, _) = facts
        .add("alex", "likes morning meetings", "user", "user")
        .await
        .expect("write");
    let _ = facts
        .add("alex", "lives in Hangzhou", "user", "user")
        .await
        .expect("write");

    let report = mirror_facts(&facts, &bundle).await.expect("the mirror");
    assert_eq!(report.written, 1);
    assert_eq!(report.skipped, 0);

    let page = bundle.read("alex").expect("read").expect("there is a page");
    assert_eq!(page.title, "alex");
    assert!(
        page.body.contains("likes morning meetings"),
        "{}",
        page.body
    );
    assert!(page.body.contains("lives in Hangzhou"), "{}", page.body);
    assert!(
        page.sources
            .iter()
            .any(|source| source == &format!("fact:{first}", first = first.id)),
        "the provenance has to point back at the fact row"
    );
    assert!(bundle
        .index_text()
        .expect("the directory")
        .contains("alex.md"));

    // The index side records it too: which page this is, and its fingerprint.
    let meta = facts.meta(first.id).await.expect("query").expect("present");
    assert_eq!(meta.page_path.as_deref(), Some("memory/alex.md"));
    assert_eq!(meta.digest.as_deref(), Some(page.digest.as_str()));
}

/// A page a human edited is not overwritten: the mirror skips it and says why.
#[tokio::test]
async fn a_hand_edited_page_is_not_overwritten() {
    let (facts, bundle) = fixtures("mirror_handedit").await;
    facts
        .add("alex", "likes morning meetings", "user", "user")
        .await
        .expect("write");
    mirror_facts(&facts, &bundle)
        .await
        .expect("the first mirror");

    // A human edits one sentence.
    let path = bundle.root().join("alex.md");
    let edited = std::fs::read_to_string(&path)
        .expect("read")
        .replace("likes morning meetings", "changed to after dinner");
    std::fs::write(&path, &edited).expect("a hand edit");

    // Another new fact arrives.
    facts
        .add("alex", "keeps a cat", "user", "user")
        .await
        .expect("write");
    let report = mirror_facts(&facts, &bundle)
        .await
        .expect("the second mirror");
    assert_eq!(report.written, 0, "it must not overwrite");
    assert_eq!(report.skipped, 1, "the skip should be recorded");

    let page = bundle.read("alex").expect("read").expect("there is a page");
    assert!(
        page.body.contains("changed to after dinner"),
        "the human's edit has to survive"
    );
    assert!(
        !page.body.contains("keeps a cat"),
        "the new fact did not reach the file this time"
    );
}

/// No facts means no empty page; nor should the index gain a line out of nowhere.
#[tokio::test]
async fn an_empty_store_writes_nothing() {
    let (facts, bundle) = fixtures("mirror_empty").await;
    let report = mirror_facts(&facts, &bundle).await.expect("the mirror");
    assert_eq!(report.written, 0);
    assert!(bundle.pages().expect("list").is_empty());
}

/// A Chinese subject needs a stable page name too (an ASCII mapping that comes out empty falls
/// back to a hash).
#[tokio::test]
async fn a_chinese_subject_gets_a_stable_slug() {
    let (facts, bundle) = fixtures("mirror_cjk").await;
    facts
        .add("张伟", "works in Shanghai", "user", "user")
        .await
        .expect("write");
    mirror_facts(&facts, &bundle).await.expect("the mirror");

    let pages = bundle.pages().expect("list");
    assert_eq!(pages.len(), 1);
    // A CJK subject keeps a CJK page name: a hand-written library should stay readable.
    assert_eq!(pages[0].slug, "张伟", "a CJK subject keeps a CJK page name");

    // Run it again and it is the same page: the name must not change every time.
    let again = mirror_facts(&facts, &bundle).await.expect("mirror again");
    assert_eq!(again.written, 1);
    assert_eq!(
        bundle.pages().expect("list").len(),
        1,
        "a second page must not appear"
    );
}

#[tokio::test]
async fn an_existing_untracked_page_is_never_claimed_as_generated_content() {
    let (facts, bundle) = fixtures("mirror_untracked").await;
    bundle.ensure().unwrap();
    let page = crate::bundle::Page::new(
        "alex",
        "fact",
        "Alex",
        "Alex",
        "Human note",
        "Keep this human-authored context.",
    );
    bundle.write(&page).unwrap();
    facts
        .add("alex", "likes morning meetings", "user", "user")
        .await
        .unwrap();
    let report = mirror_facts(&facts, &bundle).await.unwrap();
    assert_eq!(report.written, 0);
    assert_eq!(report.skipped, 1);
    assert_eq!(bundle.read("alex").unwrap().unwrap().body, page.body);
}

#[tokio::test]
async fn stale_pages_are_excluded_immediately_without_waiting_for_the_worker() {
    let (facts, bundle) = fixtures("stale_recall").await;
    let (first, _) = facts
        .add("alex", "likes tea", "user", "user")
        .await
        .unwrap();
    facts
        .add("alex", "likes walks", "user", "user")
        .await
        .unwrap();
    mirror_facts(&facts, &bundle).await.unwrap();
    let page = bundle.read("alex").unwrap().unwrap();
    assert!(super::page_is_current(&facts, &page).await.unwrap());
    facts
        .set_validity(first.id, None, Some("2000-01-01T00:00:00Z"))
        .await
        .unwrap();
    assert!(!super::page_is_current(&facts, &page).await.unwrap());
    assert!(!super::recall_catalog(&facts, &bundle)
        .await
        .unwrap()
        .contains("alex"));
    facts.set_validity(first.id, None, None).await.unwrap();
    assert!(super::page_is_current(&facts, &page).await.unwrap());
    facts.update(first.id, "likes coffee").await.unwrap();
    assert!(!super::page_is_current(&facts, &page).await.unwrap());
    facts.delete(first.id).await.unwrap();
    assert!(!super::page_is_current(&facts, &page).await.unwrap());
    assert!(bundle.read("alex").unwrap().is_some());
}
