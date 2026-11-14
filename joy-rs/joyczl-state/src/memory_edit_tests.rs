use crate::Facts;

#[tokio::test]
async fn memory_edits_are_atomic_and_invalidate_old_vectors() {
    let home = tempfile::tempdir().unwrap();
    let pool = crate::open(&home.path().join("state.db")).await.unwrap();
    let facts = Facts::new(pool.clone());
    let (row, _) = facts
        .add("person", "Old address", "consolidation", "user")
        .await
        .unwrap();
    facts.set_embedding(row.id, &[1.0, 0.0]).await.unwrap();
    sqlx::query("CREATE TRIGGER fail_suppression BEFORE INSERT ON memory_suppressions BEGIN SELECT RAISE(FAIL,'injected suppression failure'); END").execute(&pool).await.unwrap();
    assert!(facts.delete(row.id).await.is_err());
    assert!(facts.update(row.id, "New address").await.is_err());
    assert!(facts.forget_subject("person").await.is_err());
    assert_eq!(facts.search("Old address", 10).await.unwrap().len(), 1);
    assert_eq!(facts.all_with_embedding().await.unwrap().len(), 1);
    sqlx::query("DROP TRIGGER fail_suppression")
        .execute(&pool)
        .await
        .unwrap();
    facts.update(row.id, "New address").await.unwrap();
    assert!(facts.all_with_embedding().await.unwrap().is_empty());
    assert!(facts
        .add_consolidated("person", " old address ", "user")
        .await
        .unwrap()
        .is_none());
    let stored = facts.recent(10, 0).await.unwrap();
    assert_eq!(stored[0].source, "user");
    assert_eq!(stored[0].content, "New address");
    let reason: (String, i64) =
        sqlx::query_as("SELECT reason,replacement_id FROM memory_suppressions")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reason, ("corrected".into(), row.id));
}

#[tokio::test]
async fn explicit_memory_writes_do_not_reenable_automatic_subject_extraction() {
    let home = tempfile::tempdir().unwrap();
    let pool = crate::open(&home.path().join("state.db")).await.unwrap();
    let facts = Facts::new(pool.clone());
    facts.forget_subject("project").await.unwrap();
    assert!(facts
        .add_consolidated("project", "Old detail", "project")
        .await
        .unwrap()
        .is_none());
    let (row, _) = facts
        .add("project", "Explicit new detail", "user", "project")
        .await
        .unwrap();
    assert!(facts
        .add_consolidated("project", "Old detail", "project")
        .await
        .unwrap()
        .is_none());
    facts.delete(row.id).await.unwrap();
    facts
        .add("project", "Explicit new detail", "user", "project")
        .await
        .unwrap();
    assert_eq!(facts.recent(10, 0).await.unwrap().len(), 1);
}

#[tokio::test]
async fn concurrent_extraction_cannot_undo_deletion() {
    let home = tempfile::tempdir().unwrap();
    let pool = crate::open(&home.path().join("state.db")).await.unwrap();
    let facts = Facts::new(pool);
    let (row, _) = facts
        .add("person", "Old detail", "user", "user")
        .await
        .unwrap();
    let (deleted, extracted) = tokio::join!(
        facts.delete(row.id),
        facts.add_consolidated("person", "Old detail", "user")
    );
    assert!(deleted.unwrap());
    extracted.unwrap();
    assert!(facts.recent(10, 0).await.unwrap().is_empty());
}

#[tokio::test]
async fn extraction_context_is_atomic_deduplicated_and_cleared_on_correction() {
    let home = tempfile::tempdir().unwrap();
    let pool = crate::open(&home.path().join("state.db")).await.unwrap();
    let facts = Facts::new(pool.clone());
    let refs = vec!["chat:1@abc".into(), "chat:2@def".into()];
    let (row, _) = facts
        .add_consolidated_from("person", "Likes tea", "user", &refs)
        .await
        .unwrap()
        .unwrap();
    let reversed = vec![refs[1].clone(), refs[0].clone(), refs[0].clone()];
    facts
        .add_consolidated_from("person", "Likes tea", "user", &reversed)
        .await
        .unwrap();
    assert_eq!(facts.meta(row.id).await.unwrap().unwrap().confirm_count, 1);
    assert_eq!(facts.observations(row.id).await.unwrap().len(), 1);
    facts
        .add_consolidated_from("person", "Likes tea", "user", &["chat:3@ghi".into()])
        .await
        .unwrap();
    assert_eq!(facts.meta(row.id).await.unwrap().unwrap().confirm_count, 2);
    sqlx::query("CREATE TRIGGER fail_observation BEFORE INSERT ON memory_observations BEGIN SELECT RAISE(FAIL,'injected observation failure'); END").execute(&pool).await.unwrap();
    assert!(facts
        .add_consolidated_from("person", "Likes coffee", "user", &refs)
        .await
        .is_err());
    assert!(facts.search("coffee", 10).await.unwrap().is_empty());
    sqlx::query("DROP TRIGGER fail_observation")
        .execute(&pool)
        .await
        .unwrap();
    facts.update(row.id, "Likes water").await.unwrap();
    assert!(facts.observations(row.id).await.unwrap().is_empty());
    facts
        .add_consolidated_from("person", "Likes water", "user", &refs)
        .await
        .unwrap();
    facts.delete(row.id).await.unwrap();
    assert!(facts.observations(row.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn validity_filters_all_recall_paths_without_destroying_history() {
    let home = tempfile::tempdir().unwrap();
    let pool = crate::open(&home.path().join("state.db")).await.unwrap();
    let facts = Facts::new(pool);
    let (row, _) = facts
        .add("person", "Likes green tea", "user", "user")
        .await
        .unwrap();
    facts.set_embedding(row.id, &[1.0]).await.unwrap();
    facts
        .set_validity(row.id, None, Some("2000-01-01T00:00:00Z"))
        .await
        .unwrap();
    assert!(facts.search("green", 10).await.unwrap().is_empty());
    assert!(facts.search("tea", 10).await.unwrap().is_empty());
    assert!(facts.recent(10, 0).await.unwrap().is_empty());
    assert!(facts.all_with_embedding().await.unwrap().is_empty());
    assert!(facts.for_subject("person").await.unwrap().is_empty());
    assert!(facts.subjects().await.unwrap().is_empty());
    assert!(facts.all_by_subject(10).await.unwrap().is_empty());
    assert_eq!(facts.count().await.unwrap(), 1);
    assert!(facts
        .set_validity(row.id, Some("invalid"), None)
        .await
        .is_err());
    assert!(facts
        .set_validity(
            row.id,
            Some("2030-01-01T00:00:00Z"),
            Some("2000-01-01T00:00:00Z")
        )
        .await
        .is_err());
    assert_eq!(
        facts.validity(row.id).await.unwrap().1.as_deref(),
        Some("2000-01-01T00:00:00+00:00")
    );
    facts
        .set_validity(row.id, Some("2099-01-01T00:00:00Z"), None)
        .await
        .unwrap();
    assert!(facts.recent(10, 0).await.unwrap().is_empty());
    facts
        .set_validity(row.id, None, Some("2099-01-01T00:00:00Z"))
        .await
        .unwrap();
    assert_eq!(facts.recent(10, 0).await.unwrap().len(), 1);
    assert!(!facts.is_durable(row.id).await.unwrap());
    facts.set_validity(row.id, None, None).await.unwrap();
    assert!(facts.is_durable(row.id).await.unwrap());
}
