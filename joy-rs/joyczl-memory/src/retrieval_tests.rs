//! Fusion tests: cosine, RRF rank merge, and "flag off behaves exactly as before".
//!
//! The network side (actually calling the embedding service) isn't tested here — its
//! fail-open contract is guaranteed by `search_hybrid`'s embedder parameter and the
//! eprintln; real HTTP only happens when an embedding model is configured.

use joyczl_state::FactRow;

use super::retrieval::{cosine, rrf_fuse, search_hybrid};

fn row(id: i64, content: &str) -> FactRow {
    FactRow {
        id,
        subject: format!("s{id}"),
        content: content.to_string(),
        source: "test".to_string(),
        kind: "fact".to_string(),
        created_at: None,
    }
}

#[test]
fn cosine_is_the_cosine() {
    assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
    assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
    assert!((cosine(&[1.0, 1.0], &[1.0, 1.0]) - 1.0).abs() < 1e-6);
    // Degenerate inputs count as unrelated, not errors or NaN.
    assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0);
    assert_eq!(cosine(&[], &[]), 0.0);
    assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
}

#[test]
fn rrf_favours_documents_both_legs_agree_on() {
    // #2 hits both legs; #1 keyword only, #3 vector only.
    let keyword = vec![row(1, "a"), row(2, "b")];
    let semantic = vec![row(2, "b"), row(3, "c")];

    let fused = rrf_fuse(&keyword, &semantic);
    let ids: Vec<i64> = fused.iter().map(|r| r.id).collect();
    assert_eq!(ids[0], 2, "a hit on both legs comes first: {ids:?}");
    assert_eq!(
        ids.len(),
        3,
        "the union of both sides, no duplicates and nothing missing"
    );
    assert_eq!(
        fused[0].content, "b",
        "the row's content comes back as it went in"
    );
}

#[test]
fn rrf_is_stable_across_runs() {
    // Ties break by ascending id: the same query always yields the same order.
    let keyword = vec![row(7, "x")];
    let semantic = vec![row(3, "y")];
    let first: Vec<i64> = rrf_fuse(&keyword, &semantic).iter().map(|r| r.id).collect();
    let second: Vec<i64> = rrf_fuse(&keyword, &semantic).iter().map(|r| r.id).collect();
    assert_eq!(first, second);

    // With one leg only, that leg's own order stands.
    let only_keyword = rrf_fuse(&keyword, &[]);
    assert_eq!(only_keyword[0].id, 7);
}

#[tokio::test]
async fn without_an_embedder_retrieval_is_exactly_keyword() {
    let dir = tempfile::tempdir().expect("temp dir");
    let pool = joyczl_state::open(&dir.path().join("state.db"))
        .await
        .expect("open the store");
    let _ = dir.keep(); // sqlite still writes -wal/-shm: the dir must not be dropped here
    let facts = joyczl_state::Facts::new(pool);

    facts
        .add("alex", "prefers morning meetings", "user", "user")
        .await
        .unwrap();
    facts
        .add("bob", "ships on Fridays", "user", "user")
        .await
        .unwrap();

    let hybrid = search_hybrid(&facts, None, "morning", 4).await;
    let keyword = facts.search("morning", 4).await.unwrap();
    let hybrid_ids: Vec<i64> = hybrid.iter().map(|r| r.id).collect();
    let keyword_ids: Vec<i64> = keyword.iter().map(|r| r.id).collect();
    assert_eq!(
        hybrid_ids, keyword_ids,
        "with the switch off it is exactly as before the upgrade"
    );
}
