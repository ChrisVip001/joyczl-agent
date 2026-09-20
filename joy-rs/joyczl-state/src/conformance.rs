//! 契约验收 —— 任何 `SemanticStore` / `EpisodicStore` 实现都要过这一套。
//!
//! 契约存在的意义就是
//! 「每个后端跑同一套测试」。新的记忆后端写完，在它自己的测试里调
//! [`exercise_semantic`] / [`exercise_episodic`]，一次调用，全部钉住。
//!
//! 这里的断言不是 happy path：miss 不报错、重复删除返回 false、
//! 乱码查询返回空 —— 那些才是后端之间真正会漂移的地方。

use anyhow::Result;

use crate::{EpisodicStore, SemanticStore};

/// 语义记忆六动作的验收。失败 = 实现违反契约，`Err` 里说明违反了哪条。
pub async fn exercise_semantic(store: &impl SemanticStore) -> Result<()> {
    // 写入 → 检索命中。
    let fact = store.add("alex", "喜欢早上的会议", "user").await?;
    assert_eq!(fact.subject, "alex");
    let hits = store.search("alex", 4).await?;
    assert!(
        hits.iter().any(|f| f.content.contains("早上的会议")),
        "刚写入的事实必须能检索到"
    );

    // update：存在 → true 且内容真变了；缺失 → false。
    assert!(store.update(fact.id, "喜欢下午的会议").await?);
    let hits = store.search("alex", 4).await?;
    assert!(
        hits.iter().all(|f| !f.content.contains("早上的会议")),
        "改过的旧正文不该还在"
    );
    assert!(
        !store.update(fact.id + 9999, "幽灵").await?,
        "不存在的 id 必须 false"
    );

    // delete：存在 → true，再删 → false。
    assert!(store.delete(fact.id).await?);
    assert!(!store.delete(fact.id).await?, "删两次第二次必须 false");

    // forget_subject：按计数报，删光了再删是 0 —— 不报错。
    store.add("bob", "a", "user").await?;
    store.add("bob", "b", "user").await?;
    assert_eq!(store.forget_subject("bob").await?, 2);
    assert_eq!(store.forget_subject("bob").await?, 0);

    // 乱码查询：返回空而不是报错 —— 「search 的空 = 没命中」这条契约。
    assert!(store.search("???", 4).await?.is_empty());
    Ok(())
}

/// 情景记忆四动作的验收。
pub async fn exercise_episodic(store: &impl EpisodicStore) -> Result<()> {
    let id = store.add("2026-09-01", "planned the demo").await?;
    let hits = store.search("demo", 4).await?;
    assert!(
        hits.iter().any(|e| e.summary.contains("demo")),
        "刚写入的情景必须能检索到"
    );

    assert!(store.delete(id).await?);
    assert!(!store.delete(id).await?, "删两次第二次必须 false");
    assert!(
        store.search("demo", 4).await?.is_empty(),
        "删掉的就该搜不到"
    );

    // 乱码不报错。
    assert!(store.search("???", 4).await?.is_empty());
    Ok(())
}
