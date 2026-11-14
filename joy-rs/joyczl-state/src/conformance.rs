//! Contract acceptance — any `SemanticStore` / `EpisodicStore` implementation must pass this.
//!
//! The point of a contract is "every backend runs the same tests". After writing a new
//! memory backend, call [`exercise_semantic`] / [`exercise_episodic`] in its own tests:
//! one call, everything pinned.
//!
//! The assertions here are not the happy path: misses don't error, deleting twice returns
//! false, garbage queries return empty — those are where backends actually drift apart.

use anyhow::Result;

use crate::{EpisodicStore, SemanticStore};

/// Acceptance for the six semantic-memory actions. Failure = a contract violation, named in the `Err`.
pub async fn exercise_semantic(store: &impl SemanticStore) -> Result<()> {
    // Write → search hits.
    let fact = store
        .add("alex", "likes the morning meeting", "user", "user")
        .await?;
    assert_eq!(fact.subject, "alex");
    let hits = store.search("alex", 4).await?;
    assert!(
        hits.iter()
            .any(|f| f.content.contains("the morning meeting")),
        "a fact that was just written must be findable again"
    );

    // update: exists → true and content really changed; missing → false.
    assert!(store.update(fact.id, "likes the afternoon meeting").await?);
    let hits = store.search("alex", 4).await?;
    assert!(
        hits.iter()
            .all(|f| !f.content.contains("the morning meeting")),
        "the edited body must be gone"
    );
    assert!(
        !store.update(fact.id + 9999, "ghost").await?,
        "an id that does not exist must return false"
    );

    // delete: exists → true, deleting again → false.
    assert!(store.delete(fact.id).await?);
    assert!(
        !store.delete(fact.id).await?,
        "deleting twice must return false the second time"
    );

    // forget_subject: reports a count; deleting an emptied subject is 0 — no error.
    store.add("bob", "a", "user", "fact").await?;
    store.add("bob", "b", "user", "fact").await?;
    assert_eq!(store.forget_subject("bob").await?, 2);
    assert_eq!(store.forget_subject("bob").await?, 0);

    // Garbage query: returns empty instead of erroring — the "empty search = no hit" contract.
    assert!(store.search("???", 4).await?.is_empty());
    Ok(())
}

/// Acceptance for the four episodic-memory actions.
pub async fn exercise_episodic(store: &impl EpisodicStore) -> Result<()> {
    let id = store.add("2026-09-01", "planned the demo").await?;
    let hits = store.search("demo", 4).await?;
    assert!(
        hits.iter().any(|e| e.summary.contains("demo")),
        "an episode that was just written must be findable again"
    );

    assert!(store.delete(id).await?);
    assert!(
        !store.delete(id).await?,
        "deleting twice must return false the second time"
    );
    assert!(
        store.search("demo", 4).await?.is_empty(),
        "what was deleted must not be findable"
    );

    // Garbage does not error.
    assert!(store.search("???", 4).await?.is_empty());
    Ok(())
}
