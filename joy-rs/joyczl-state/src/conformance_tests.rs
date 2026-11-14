//! Contract acceptance runs on the homegrown SQLite implementation. Every future memory
//! backend (pgvector, hosted API…) calls the same `exercise_*` in its own tests — not one
//! line more, not one contract clause missed.

use super::conformance::{exercise_episodic, exercise_semantic};
use super::store::SemanticStore;
use crate::{Episodes, Facts};

async fn stores() -> (Facts, Episodes) {
    let dir = tempfile::tempdir().expect("temp dir");
    let pool = crate::open(&dir.path().join("state.db"))
        .await
        .expect("open the store");
    let _ = dir.keep(); // sqlite still writes -wal/-shm: the dir must not be dropped here
    (Facts::new(pool.clone()), Episodes::new(pool))
}

#[tokio::test]
async fn sqlite_semantic_store_honours_the_contract() {
    let (facts, _episodes) = stores().await;
    exercise_semantic(&facts)
        .await
        .expect("an in-house implementation must honour its own contract");
}

#[tokio::test]
async fn sqlite_episodic_store_honours_the_contract() {
    let (_facts, episodes) = stores().await;
    exercise_episodic(&episodes)
        .await
        .expect("an in-house implementation must honour its own contract");
}

/// When a trait and inherent methods coexist, the inherent ones must still work —
/// a regression test for the delegation implementations themselves.
#[tokio::test]
async fn trait_objects_and_inherent_calls_agree() {
    let (facts, _episodes) = stores().await;
    let fact = SemanticStore::add(&facts, "alex", "likes mornings", "user", "user")
        .await
        .unwrap();
    assert_eq!(facts.search("mornings", 4).await.unwrap().len(), 1);
    assert!(SemanticStore::delete(&facts, fact.id).await.unwrap());
}
