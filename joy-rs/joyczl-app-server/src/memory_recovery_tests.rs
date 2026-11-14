//! Recovery of memory work must not confuse an admitted batch with a completed one.
#[tokio::test]
async fn startup_recovers_a_memory_claim_interrupted_before_completion() {
    let home = tempfile::tempdir().unwrap();
    let pool = joyczl_state::open(&home.path().join("state.db"))
        .await
        .unwrap();
    let chat = joyczl_state::Chat::new(pool.clone());
    chat.append_exchange(
        "Please remember my preferred name.",
        "Noted.",
        "chat",
        "test",
        None,
    )
    .await
    .unwrap();
    assert_eq!(chat.claim_unconsolidated().await.unwrap().len(), 2);
    drop(chat);
    pool.close().await;
    let pool = joyczl_state::open(&home.path().join("state.db"))
        .await
        .unwrap();
    let server = crate::Server::boot(
        pool,
        joyczl_config::Settings {
            home: home.path().into(),
            provider: "test-unconfigured".into(),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        server.chat.unconsolidated().await.unwrap().len(),
        2,
        "An interrupted memory claim must be available after restart"
    );
}

#[tokio::test]
async fn a_live_memory_worker_retains_database_execution_ownership() {
    let home = tempfile::tempdir().unwrap();
    let settings = joyczl_config::Settings {
        home: home.path().into(),
        provider: "test-unconfigured".into(),
        ..Default::default()
    };
    let mut server = crate::open(&settings).await.unwrap();
    server.attach_memory_worker();
    let worker = server.memory_worker.as_ref().unwrap().clone();
    drop(server);
    assert!(
        crate::open(&settings).await.is_err(),
        "A live memory worker must retain its runtime's ownership"
    );
    worker.stop().await;
    drop(worker);
    assert!(crate::open(&settings).await.is_ok());
}

struct PendingExtraction {
    started: std::sync::Arc<tokio::sync::Notify>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl joyczl_provider::Provider for PendingExtraction {
    fn create(
        &self,
        _: joyczl_provider::CreateRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        joyczl_provider::CreateResponse,
                        joyczl_provider::ProviderError,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            struct Mark(std::sync::Arc<std::sync::atomic::AtomicBool>);
            impl Drop for Mark {
                fn drop(&mut self) {
                    self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let _mark = Mark(self.cancelled.clone());
            self.started.notify_one();
            std::future::pending().await
        })
    }
}

#[tokio::test]
async fn stopping_a_stalled_memory_worker_cancels_and_joins_its_model_future() {
    let home = tempfile::tempdir().unwrap();
    let pool = joyczl_state::open(&home.path().join("state.db"))
        .await
        .unwrap();
    let mut server = crate::Server::boot(
        pool,
        joyczl_config::Settings {
            home: home.path().into(),
            provider: "test-unconfigured".into(),
            consolidate_every: 1,
            ..Default::default()
        },
    )
    .await;
    let started = std::sync::Arc::new(tokio::sync::Notify::new());
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    server.install_provider(joyczl_provider::Resolved {
        provider_id: "test".into(),
        model: "test".into(),
        small_model: "test".into(),
        vision: false,
        context_window: joyczl_provider::DEFAULT_CONTEXT_WINDOW,
        client: std::sync::Arc::new(PendingExtraction {
            started: started.clone(),
            cancelled: cancelled.clone(),
        }),
    });
    server
        .chat
        .append_exchange("My name is Lin.", "Hello, Lin.", "chat", "test", None)
        .await
        .unwrap();
    server.attach_memory_worker();
    server.memory_worker.as_ref().unwrap().wake();
    tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    server.stop_memory_worker().await;
    assert!(
        cancelled.load(std::sync::atomic::Ordering::SeqCst),
        "Shutdown must cancel and join, not detach the memory model future"
    );
}

#[tokio::test]
async fn recovery_failure_retains_memory_claims_until_a_later_successful_start() {
    let home = tempfile::tempdir().unwrap();
    let pool = joyczl_state::open(&home.path().join("state.db"))
        .await
        .unwrap();
    let chat = joyczl_state::Chat::new(pool.clone());
    chat.append_exchange("Call me Lin.", "Hello, Lin.", "chat", "test", None)
        .await
        .unwrap();
    chat.claim_unconsolidated().await.unwrap();
    sqlx::query("CREATE TRIGGER fail_memory_restore BEFORE UPDATE ON chat_log WHEN OLD.consolidation_claimed_at IS NOT NULL AND NEW.consolidated=0 BEGIN SELECT RAISE(FAIL,'injected memory recovery failure'); END").execute(&pool).await.unwrap();
    let settings = joyczl_config::Settings {
        home: home.path().into(),
        provider: "test-unconfigured".into(),
        ..Default::default()
    };
    let server = crate::Server::boot(pool.clone(), settings.clone()).await;
    let retained: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_log WHERE consolidated=1 AND consolidation_claimed_at IS NOT NULL").fetch_one(&pool).await.unwrap();
    assert_eq!(retained, 2);
    assert_eq!(server.chat.session_history("chat").await.unwrap().len(), 1);
    drop(server);
    sqlx::query("DROP TRIGGER fail_memory_restore")
        .execute(&pool)
        .await
        .unwrap();
    let server = crate::Server::boot(pool, settings).await;
    assert_eq!(server.chat.unconsolidated().await.unwrap().len(), 2);
}
