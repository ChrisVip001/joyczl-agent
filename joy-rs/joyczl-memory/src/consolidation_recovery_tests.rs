//! Real SQLite boundaries between source admission, derived writes and completion.
use super::consolidation::consolidate_if_due;
use joyczl_provider::mock::Mock;
use joyczl_state::{Chat, Episodes, Facts};

async fn fixture() -> (tempfile::TempDir, sqlx::SqlitePool, Chat, Facts, Episodes) {
    let home = tempfile::tempdir().unwrap();
    let pool = joyczl_state::open(&home.path().join("state.db"))
        .await
        .unwrap();
    let chat = Chat::new(pool.clone());
    chat.append_exchange("Call me Lin.", "Hello, Lin.", "chat", "test", None)
        .await
        .unwrap();
    (
        home,
        pool.clone(),
        chat,
        Facts::new(pool.clone()),
        Episodes::new(pool),
    )
}

#[tokio::test]
async fn a_recovered_batch_keeps_its_sources_and_ignores_the_new_work_threshold() {
    let (_home, pool, chat, facts, episodes) = fixture().await;
    let claimed = chat.claim_consolidation(2).await.unwrap();
    assert_eq!(claimed.len(), 2);
    let batches: i64 =
        sqlx::query_scalar("SELECT COUNT(DISTINCT consolidation_batch) FROM chat_log")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        batches, 1,
        "All admitted source rows must have one stable batch identity"
    );
    assert_eq!(chat.recover_consolidation_claims().await.unwrap(), 2);
    chat.append_exchange(
        "New conversation marker.",
        "New answer marker.",
        "chat",
        "test",
        None,
    )
    .await
    .unwrap();
    let mock = Mock::new(vec![Mock::text(
        r#"{"facts":[{"subject":"name","content":"Prefers Lin","kind":"user"}],"episode":"Discussed the preferred name."}"#,
    )]);
    assert_eq!(
        consolidate_if_due(&chat, &facts, &episodes, &mock, "small", 1_000)
            .await
            .unwrap(),
        1
    );
    {
        let requests = mock.received.lock().unwrap();
        let input: serde_json::Value =
            serde_json::from_str(&requests[0].messages[0].text()).unwrap();
        assert_eq!(input.as_array().unwrap().len(), 2);
        assert!(!requests[0].messages[0]
            .text()
            .contains("New conversation marker"));
    }
    assert_eq!(chat.unconsolidated().await.unwrap().len(), 2);
    assert!(chat.claim_consolidation(1_000).await.unwrap().is_empty());
    assert_eq!(
        chat.recover_consolidation_claims().await.unwrap(),
        0,
        "Completed source rows must stay completed"
    );
}

#[tokio::test]
async fn failure_after_derived_writes_retries_without_duplicate_facts_or_episodes() {
    let (_home, pool, chat, facts, episodes) = fixture().await;
    sqlx::query("CREATE TRIGGER fail_batch_finish BEFORE UPDATE ON chat_log WHEN OLD.consolidation_claimed_at IS NOT NULL AND NEW.consolidation_claimed_at IS NULL AND NEW.consolidated=1 BEGIN SELECT RAISE(FAIL,'injected batch completion failure'); END").execute(&pool).await.unwrap();
    let first = Mock::new(vec![Mock::text(
        r#"{"facts":[{"subject":"name","content":"Prefers Lin","kind":"user"}],"episode":"Original committed summary."}"#,
    )]);
    assert!(
        consolidate_if_due(&chat, &facts, &episodes, &first, "small", 1)
            .await
            .is_err()
    );
    assert_eq!(facts.count().await.unwrap(), 1);
    assert_eq!(episodes.recent(10).await.unwrap().len(), 1);
    sqlx::query("DROP TRIGGER fail_batch_finish")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE chat_log SET consolidation_next_at=NULL")
        .execute(&pool)
        .await
        .unwrap();
    let retry = Mock::new(vec![Mock::text(
        r#"{"facts":[{"subject":"name","content":"Prefers Lin","kind":"user"}],"episode":"A different retry summary must not replace the receipt."}"#,
    )]);
    assert_eq!(
        consolidate_if_due(&chat, &facts, &episodes, &retry, "small", 1)
            .await
            .unwrap(),
        0
    );
    let rows = facts.recent(10, 0).await.unwrap();
    assert_eq!(
        facts.meta(rows[0].id).await.unwrap().unwrap().confirm_count,
        1
    );
    let summaries = episodes.recent(10).await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].summary, "Original committed summary.");
    assert!(chat.unconsolidated().await.unwrap().is_empty());
    assert_eq!(chat.recover_consolidation_claims().await.unwrap(), 0);
}

#[tokio::test]
async fn batch_backoff_is_atomic_if_updating_one_source_row_fails() {
    let (_home, pool, chat, _facts, _episodes) = fixture().await;
    let rows = chat.claim_unconsolidated().await.unwrap();
    let ids: Vec<i64> = rows.iter().map(|row| row.0).collect();
    sqlx::query("CREATE TRIGGER fail_backoff BEFORE UPDATE ON chat_log WHEN NEW.id=2 AND NEW.consolidated=0 BEGIN SELECT RAISE(FAIL,'injected source backoff failure'); END").execute(&pool).await.unwrap();
    assert!(chat.mark_consolidation_failed(&ids).await.is_err());
    let claimed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_log WHERE consolidated=1 AND consolidation_claimed_at IS NOT NULL").fetch_one(&pool).await.unwrap();
    assert_eq!(claimed, 2, "A failed backoff must not split the batch");
    sqlx::query("DROP TRIGGER fail_backoff")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(chat.recover_consolidation_claims().await.unwrap(), 2);
    assert_eq!(chat.claim_consolidation(1_000).await.unwrap().len(), 2);
}

#[tokio::test]
async fn episode_receipts_survive_human_deletion_and_canonicalize_source_order() {
    let (_home, pool, _chat, _facts, episodes) = fixture().await;
    let refs = ["chat:1@first".into(), "chat:2@second".into()];
    assert!(episodes
        .add_consolidated("2026-11-14", "Original", &refs)
        .await
        .unwrap());
    assert!(!episodes
        .add_consolidated(
            "2026-11-14",
            "Replacement",
            &[refs[1].clone(), refs[0].clone()]
        )
        .await
        .unwrap());
    sqlx::query("DELETE FROM episodes")
        .execute(&pool)
        .await
        .unwrap();
    assert!(!episodes
        .add_consolidated("2026-11-14", "Resurrected", &refs)
        .await
        .unwrap());
    assert!(episodes.recent(10).await.unwrap().is_empty());
}
