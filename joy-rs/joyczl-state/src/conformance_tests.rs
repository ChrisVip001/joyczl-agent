//! 契约验收跑在本家的 SQLite 实现上。将来的每个记忆后端（pgvector、
//! 托管 API……）在自己的测试里调用同一套 `exercise_*` —— 一行不多写，
//! 契约一次不漏。

use super::conformance::{exercise_episodic, exercise_semantic};
use super::store::SemanticStore;
use crate::{Episodes, Facts};

async fn stores() -> (Facts, Episodes) {
    let dir = tempfile::tempdir().expect("临时目录");
    let pool = crate::open(&dir.path().join("state.db"))
        .await
        .expect("打开库");
    std::mem::forget(dir); // sqlite 要写 -wal/-shm，目录不能提前消失
    (Facts::new(pool.clone()), Episodes::new(pool))
}

#[tokio::test]
async fn sqlite_semantic_store_honours_the_contract() {
    let (facts, _episodes) = stores().await;
    exercise_semantic(&facts)
        .await
        .expect("本家实现不得违反自己定的契约");
}

#[tokio::test]
async fn sqlite_episodic_store_honours_the_contract() {
    let (_facts, episodes) = stores().await;
    exercise_episodic(&episodes)
        .await
        .expect("本家实现不得违反自己定的契约");
}

/// trait 与固有方法并存时，固有方法必须仍然照常工作 ——
/// 这是对「委托实现」本身的回归测试。
#[tokio::test]
async fn trait_objects_and_inherent_calls_agree() {
    let (facts, _episodes) = stores().await;
    let fact = SemanticStore::add(&facts, "alex", "likes mornings", "user")
        .await
        .unwrap();
    assert_eq!(facts.search("mornings", 4).await.unwrap().len(), 1);
    assert!(SemanticStore::delete(&facts, fact.id).await.unwrap());
}
