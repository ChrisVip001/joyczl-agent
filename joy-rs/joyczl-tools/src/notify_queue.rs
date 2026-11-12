//! Durable completion notifications. Delivery is at least once: a remote acknowledgement
//! followed by a local crash can produce a duplicate. Approval requests are never queued.
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

static NEXT: AtomicU64 = AtomicU64::new(0);

pub(crate) struct Queue {
    dir: PathBuf,
    url: String,
    client: reqwest::Client,
    started: AtomicBool,
    wake: tokio::sync::Notify,
}

impl Queue {
    pub(crate) fn new(home: &Path, url: &str) -> Arc<Self> {
        // Bind pending receipts to their configured destination without storing the webhook
        // credential. This fingerprint is an accidental-config-change fence, not authentication.
        let fingerprint = url.bytes().fold(0xcbf29ce484222325u64, |hash, b| {
            (hash ^ u64::from(b)).wrapping_mul(0x100000001b3)
        });
        Arc::new(Self {
            dir: home
                .join("outbox")
                .join("notifications")
                .join(format!("{fingerprint:016x}")),
            url: url.to_string(),
            client: reqwest::Client::new(),
            started: AtomicBool::new(false),
            wake: tokio::sync::Notify::new(),
        })
    }

    pub(crate) fn target(&self) -> String {
        self.dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }

    /// Commit before returning to the caller, before any network request can begin.
    pub(crate) fn enqueue(self: &Arc<Self>, text: &str) -> std::io::Result<()> {
        use std::io::Write;
        std::fs::create_dir_all(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let id = format!(
            "{stamp}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let temporary = self.dir.join(format!("{id}.tmp"));
        let committed = self.dir.join(format!("{id}.json"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(serde_json::json!({"text": text}).to_string().as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, &committed)?;
        #[cfg(unix)]
        std::fs::File::open(&self.dir)?.sync_all()?;
        self.start();
        self.wake.notify_one();
        Ok(())
    }

    pub(crate) fn start(self: &Arc<Self>) {
        // Configuration can be inspected outside a runtime. The first actual notification
        // retries starting the worker, and normal server boot already runs inside Tokio.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(self);
        runtime.spawn(async move {
            loop {
                let Some(queue) = weak.upgrade() else {
                    break;
                };
                queue.flush().await;
                tokio::select! {
                    _ = queue.wake.notified() => {},
                    _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {},
                }
                // Do not keep an otherwise unused notifier alive indefinitely.
                if Arc::strong_count(&queue) == 1 {
                    break;
                }
            }
        });
    }

    async fn flush(&self) {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                eprintln!("(joy) notification outbox unavailable: {e}");
                return;
            }
        };
        // A failing destination must not create an unbounded burst each sweep.
        for entry in entries
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .take(32)
        {
            let path = entry.path();
            let payload = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
            let Some(payload) = payload.filter(|p| p.get("text").is_some_and(|t| t.is_string()))
            else {
                eprintln!(
                    "(joy) malformed pending notification retained: {}",
                    path.display()
                );
                continue;
            };
            match self
                .client
                .post(&self.url)
                .json(&payload)
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => {
                    if let Err(e) = std::fs::remove_file(&path) {
                        eprintln!(
                            "(joy) notification delivered but receipt could not be cleared: {e}"
                        );
                    }
                }
                Ok(response) => {
                    eprintln!(
                        "(joy) notification retained for retry: HTTP {}",
                        response.status()
                    );
                    break;
                }
                // reqwest errors can contain the credential-bearing webhook URL.
                Err(_) => {
                    eprintln!("(joy) notification retained for retry: transport failure");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_is_committed_before_return_without_storing_credentials() {
        let home = tempfile::tempdir().unwrap();
        let queue = Queue::new(home.path(), "https://example.invalid/secret-webhook");
        queue.enqueue("job finished").unwrap();
        let files: Vec<_> = std::fs::read_dir(&queue.dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        let body = std::fs::read_to_string(files[0].path()).unwrap();
        assert!(body.contains("job finished"));
        assert!(!body.contains("secret-webhook"));
        assert_eq!(files[0].path().extension().unwrap(), "json");
        let different = Queue::new(home.path(), "https://example.invalid/other-target");
        assert_ne!(queue.dir, different.dir);
    }

    #[tokio::test]
    async fn failed_delivery_survives_recreation_and_is_cleared_only_on_ack() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let remote = tokio::spawn(async move {
            for status in ["500 Internal Server Error", "200 OK"] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = [0; 4096];
                // A mock server: it only needs to know a request arrived. `read_exact`
                // would wait for 4096 bytes that never come, so handle the amount instead.
                let n = stream.read(&mut bytes).await.unwrap();
                assert!(n > 0, "the mock server should receive an HTTP request");
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        let home = tempfile::tempdir().unwrap();
        let queue = Queue::new(home.path(), &url);
        let writer = queue.clone();
        // No runtime in this thread: test the persisted receipt before starting delivery.
        std::thread::spawn(move || writer.enqueue("finished").unwrap())
            .join()
            .unwrap();
        queue.flush().await;
        assert_eq!(std::fs::read_dir(&queue.dir).unwrap().count(), 1);
        drop(queue);
        let restored = Queue::new(home.path(), &url);
        restored.flush().await;
        assert_eq!(std::fs::read_dir(&restored.dir).unwrap().count(), 0);
        remote.await.unwrap();
    }
}
