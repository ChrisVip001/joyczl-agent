use super::*;
use crate::{Chat, Facts};

async fn fixture() -> (
    tempfile::TempDir,
    sqlx::SqlitePool,
    Facts,
    TransientObservation,
) {
    let home = tempfile::tempdir().unwrap();
    let pool = crate::open(&home.path().join("state.db")).await.unwrap();
    Chat::new(pool.clone())
        .append_exchange(
            "I feel exhausted today.",
            "I hear you.",
            "chat",
            "test",
            None,
        )
        .await
        .unwrap();
    let digest = crate::ledger::digest_of(
        serde_json::to_string(&("user", "I feel exhausted today."))
            .unwrap()
            .as_bytes(),
    );
    let observation = TransientObservation {
        source_ref: format!("chat:1@{digest}"),
        quote: "feel exhausted today".into(),
        confidence: 0.8,
        ttl_hours: 4,
    };
    (home, pool.clone(), Facts::new(pool), observation)
}

#[tokio::test]
async fn temporary_claims_expire_from_source_time_and_retries_do_not_renew_them() {
    let (_home, pool, facts, observation) = fixture().await;
    sqlx::query("UPDATE chat_log SET created_at=datetime('now','-1 hour') WHERE id=1")
        .execute(&pool)
        .await
        .unwrap();
    let refs = vec![observation.source_ref.clone()];
    let (row, fresh) = facts
        .add_transient_from("user", "Feels exhausted today", "user", &refs, &observation)
        .await
        .unwrap()
        .unwrap();
    assert!(fresh);
    assert!(!facts.is_durable(row.id).await.unwrap());
    assert_eq!(facts.search("exhausted", 5).await.unwrap().len(), 1);
    let validity = facts.validity(row.id).await.unwrap();
    let until = chrono::DateTime::parse_from_rfc3339(validity.1.as_deref().unwrap()).unwrap();
    let hours = (until.to_utc() - chrono::Utc::now()).num_minutes();
    assert!(
        (175..=180).contains(&hours),
        "Expiry must follow the source message, not extraction time"
    );
    facts
        .add_transient_from("user", "Feels exhausted today", "user", &refs, &observation)
        .await
        .unwrap();
    assert_eq!(facts.validity(row.id).await.unwrap(), validity);
    assert_eq!(facts.meta(row.id).await.unwrap().unwrap().confirm_count, 1);
    let observations = facts.observations_detailed(row.id).await.unwrap();
    let assessment: serde_json::Value =
        serde_json::from_str(observations[0].3.as_deref().unwrap()).unwrap();
    assert_eq!(assessment["confidence"], 0.8);
    assert_eq!(assessment["quote"], observation.quote);
    assert_eq!(assessment["validUntil"], validity.1.unwrap());
}

#[tokio::test]
async fn expired_sources_and_invalid_claims_never_create_durable_memories() {
    let (_home, pool, facts, original) = fixture().await;
    let refs = vec![original.source_ref.clone()];
    for invalid in [
        TransientObservation {
            quote: "a quote never said".into(),
            ..original.clone()
        },
        TransientObservation {
            confidence: 1.1,
            ..original.clone()
        },
        TransientObservation {
            ttl_hours: 25,
            ..original.clone()
        },
        TransientObservation {
            source_ref: "chat:1@wrong-digest".into(),
            ..original.clone()
        },
    ] {
        assert!(facts
            .add_transient_from("user", "exhausted", "user", &refs, &invalid)
            .await
            .is_err());
    }
    assert!(facts
        .add_transient_from("user", "exhausted", "user", &[], &original)
        .await
        .is_err());
    sqlx::query("UPDATE chat_log SET created_at=datetime('now','-2 days') WHERE id=1")
        .execute(&pool)
        .await
        .unwrap();
    assert!(facts
        .add_transient_from("user", "exhausted", "user", &refs, &original)
        .await
        .unwrap()
        .is_none());
    assert_eq!(facts.count().await.unwrap(), 0);
}

#[tokio::test]
async fn human_validity_edits_and_deletions_cannot_be_overridden_by_extraction() {
    let (_home, _pool, facts, observation) = fixture().await;
    let refs = vec![observation.source_ref.clone()];
    let (row, _) = facts
        .add_transient_from("user", "exhausted", "user", &refs, &observation)
        .await
        .unwrap()
        .unwrap();
    facts
        .set_validity(row.id, None, Some("2000-01-01T00:00:00Z"))
        .await
        .unwrap();
    let validity = facts.validity(row.id).await.unwrap();
    assert!(facts
        .add_transient_from("user", "exhausted", "user", &refs, &observation)
        .await
        .unwrap()
        .is_none());
    assert_eq!(facts.validity(row.id).await.unwrap(), validity);
    facts.delete(row.id).await.unwrap();
    assert!(facts
        .add_transient_from("user", "exhausted", "user", &refs, &observation)
        .await
        .unwrap()
        .is_none());
    assert!(facts
        .observations_detailed(row.id)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a_failed_assessment_commit_rolls_back_the_fact_and_validity() {
    let (_home, pool, facts, observation) = fixture().await;
    sqlx::query("CREATE TRIGGER fail_assessment BEFORE INSERT ON memory_transient_assessments BEGIN SELECT RAISE(FAIL,'injected assessment failure'); END").execute(&pool).await.unwrap();
    assert!(facts
        .add_transient_from(
            "user",
            "exhausted",
            "user",
            std::slice::from_ref(&observation.source_ref),
            &observation
        )
        .await
        .is_err());
    assert_eq!(facts.count().await.unwrap(), 0);
    let bounds: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memory_validity")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(bounds, 0);
}

#[tokio::test]
async fn temporary_observations_require_authentic_user_sources_and_preserve_legacy_claims() {
    let (_home, pool, facts, original) = fixture().await;
    let assistant = crate::ledger::digest_of(
        serde_json::to_string(&("assistant", "I hear you."))
            .unwrap()
            .as_bytes(),
    );
    for invalid in [
        TransientObservation {
            source_ref: format!("chat:2@{assistant}"),
            quote: "I hear you.".into(),
            ..original.clone()
        },
        TransientObservation {
            source_ref: "chat:1@wrong-digest".into(),
            ..original.clone()
        },
    ] {
        let refs = vec![invalid.source_ref.clone()];
        assert!(facts
            .add_transient_from("user", "exhausted", "user", &refs, &invalid)
            .await
            .is_err());
    }
    let refs = vec![original.source_ref.clone()];
    let (row, _) = facts
        .add_consolidated_from("user", "exhausted", "user", &refs)
        .await
        .unwrap()
        .unwrap();
    assert!(facts
        .add_transient_from("user", "exhausted", "user", &refs, &original)
        .await
        .unwrap()
        .is_none());
    assert!(
        facts.is_durable(row.id).await.unwrap(),
        "Legacy unbounded claims require human review"
    );
    sqlx::query("UPDATE chat_log SET created_at=datetime('now','+1 day') WHERE id=1")
        .execute(&pool)
        .await
        .unwrap();
    assert!(facts
        .add_transient_from("user", "a new state", "user", &refs, &original)
        .await
        .is_err());
}
