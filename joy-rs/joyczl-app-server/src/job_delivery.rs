//! Produce completion notices from outcome events committed with the job ledger.
use joyczl_tools::notify::{Notifier, NotifyEvent};
use std::sync::Arc;

pub(crate) fn start(
    rows: &Arc<joyczl_state::Jobs>,
    notifier: &Arc<dyn Notifier>,
    lock: &Arc<tokio::sync::Mutex<()>>,
) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let (rows, notifier, lock) = (
        Arc::downgrade(rows),
        Arc::downgrade(notifier),
        Arc::downgrade(lock),
    );
    runtime.spawn(async move {
        loop {
            {
                let (Some(rows), Some(notifier), Some(lock)) =
                    (rows.upgrade(), notifier.upgrade(), lock.upgrade())
                else {
                    break;
                };
                let _guard = lock.lock().await;
                deliver(&rows, notifier.as_ref()).await;
            }
            // Weak handles ensure idle/evicted account servers do not retain their pools.
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}

pub(crate) async fn deliver(rows: &joyczl_state::Jobs, notifier: &dyn Notifier) {
    let pending = match rows.pending_notifications().await {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("(joy) job completion outbox unavailable: {e}");
            return;
        }
    };
    let target = notifier.delivery_target();
    for row in pending {
        if row.notify_target != target {
            eprintln!(
                "(joy) job {} completion retained: notification destination changed",
                row.id
            );
            continue;
        }
        let event = NotifyEvent::JobFinished {
            id: row.id.clone(),
            command: row.command,
            status: row.status,
            exit_code: row.exit_code,
        };
        match notifier.enqueue(event) {
            Ok(()) => {
                if let Err(e) = rows.notification_enqueued(&row.id).await {
                    eprintln!(
                        "(joy) job {} notification receipt not recorded: {e}",
                        row.id
                    );
                }
            }
            Err(e) => eprintln!("(joy) job {} completion retained for retry: {e}", row.id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Sink {
        target: String,
        fail: bool,
        seen: std::sync::Mutex<Vec<String>>,
    }
    impl Notifier for Sink {
        fn notify(&self, _: NotifyEvent) {
            panic!("must use acknowledged enqueue")
        }
        fn delivery_target(&self) -> Option<String> {
            Some(self.target.clone())
        }
        fn enqueue(&self, event: NotifyEvent) -> Result<(), String> {
            if self.fail {
                return Err("injected persistence failure".into());
            }
            self.seen.lock().unwrap().push(event.message());
            Ok(())
        }
    }
    #[tokio::test]
    async fn committed_job_completion_retries_enqueue_and_keeps_its_destination() {
        let home = tempfile::tempdir().unwrap();
        let pool = joyczl_state::open(&home.path().join("state.db"))
            .await
            .unwrap();
        let rows = joyczl_state::Jobs::new(pool.clone());
        rows.record_started_with_delivery("j", "s", "echo", "log", None, true, Some("original"))
            .await
            .unwrap();
        rows.record_finished("j", "completed", Some(0))
            .await
            .unwrap();
        let failed = Sink {
            target: "original".into(),
            fail: true,
            seen: Default::default(),
        };
        deliver(&rows, &failed).await;
        assert_eq!(rows.pending_notifications().await.unwrap().len(), 1);
        drop(rows);
        let restored = joyczl_state::Jobs::new(pool);
        let changed = Sink {
            target: "changed".into(),
            fail: false,
            seen: Default::default(),
        };
        deliver(&restored, &changed).await;
        assert!(changed.seen.lock().unwrap().is_empty());
        assert_eq!(restored.pending_notifications().await.unwrap().len(), 1);
        let accepted = Sink {
            target: "original".into(),
            fail: false,
            seen: Default::default(),
        };
        deliver(&restored, &accepted).await;
        deliver(&restored, &accepted).await;
        assert_eq!(accepted.seen.lock().unwrap().len(), 1);
        assert!(restored.pending_notifications().await.unwrap().is_empty());
    }
}
