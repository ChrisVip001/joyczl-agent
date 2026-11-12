//! The background memory worker: distillation, mirroring, and nothing on the turn's clock.
//!
//! Before this, `finalize_turn` did three memory jobs *after* the reply was persisted but
//! still inside the turn — a model call for consolidation, a full rewrite of MEMORY.md,
//! and the page mirror. The user had their answer, yet the turn was not finished: it held
//! the session's write lock and the process's attention for seconds more, and a slow
//! provider made "the reply arrived but nothing else works" a real state.
//!
//! Now the turn ends when the reply is on disk. A worker — one per process — waits for a
//! nudge (or a 60 s heartbeat), asks "is anything due?", and does the same work:
//!
//! * **Count-triggered**: exactly like `chat_log`'s own rule, consolidation runs when
//!   enough new rows have accumulated and does nothing otherwise — an idle Joy burns no
//!   tokens and writes no files.
//! * **Single-flight across processes**: the claim in `claim_unconsolidated` is the same
//!   atomic UPDATE…RETURNING; two hubs sharing state.db cannot both distill the same rows.
//! * **Never silent**: every run that changes something says so on stderr, and the count
//!   is carried to the next turn so the client still gets `consolidationCompleted`.
//! * **Nothing is lost on shutdown**: a claim taken but not finished is handed back with
//!   the existing backoff, and a turn's ledger row is already committed before the nudge.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use joyczl_config::Settings;
use joyczl_provider::Resolved;
use joyczl_state::{Chat, Episodes, Facts};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// How often the worker checks even when nobody nudges it. A turn wakes it immediately;
/// this is what catches "the app sat idle and rows accumulated elsewhere".
const HEARTBEAT: Duration = Duration::from_secs(60);

/// How long `stop()` waits for a run in flight. A claim that outlives it stays `claimed`
/// and the next process picks it up after the backoff — losing an in-flight distillation
/// is recoverable; blocking shutdown is not.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Everything the worker touches. Handles only — no `Server`, so the worker cannot drift
/// into "and also run turns".
pub struct Deps {
    pub chat: Chat,
    pub facts: Facts,
    pub episodes: Episodes,
    pub settings: Arc<std::sync::RwLock<Settings>>,
    pub resolved: Arc<std::sync::RwLock<Option<Resolved>>>,
}

pub struct MemoryWorker {
    stop: Arc<AtomicBool>,
    wake: Arc<Notify>,
    /// Facts distilled but not yet reported to a client (see `take_pending`).
    pending: Arc<AtomicI32>,
    handle: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl MemoryWorker {
    pub fn spawn(deps: Deps) -> Arc<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(Notify::new());
        let pending = Arc::new(AtomicI32::new(0));

        let worker = Arc::new(Self {
            handle: std::sync::Mutex::new(None),
            stop: stop.clone(),
            wake: wake.clone(),
            pending: pending.clone(),
        });

        let handle = tokio::spawn(async move {
            // `last_facts` avoids rewriting every page and MEMORY.md on a heartbeat where
            // nothing changed: the mirror is cheap, but file mtimes are not free.
            let mut last_facts = -1i64;
            loop {
                tokio::select! {
                    _ = wake.notified() => {}
                    _ = tokio::time::sleep(HEARTBEAT) => {}
                }
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                if let Err(error) = run_once(&deps, &pending, &mut last_facts).await {
                    eprintln!("(joy) memory consolidation did not get done this round (trying again next round): {error}");
                }
            }
        });
        *worker
            .handle
            .lock()
            .expect("the worker-handle lock should not be poisoned") = Some(handle);
        worker
    }

    /// "Something happened; check whether memory work is due." Never blocks the caller.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// Facts distilled since the last client was told, and reset. The next turn carries
    /// the `consolidationCompleted` notification instead of the turn that caused it —
    /// which is also when the user can act on it.
    pub fn take_pending(&self) -> i32 {
        self.pending.swap(0, Ordering::SeqCst)
    }

    /// Stop and wait (up to `SHUTDOWN_GRACE`) for a run in flight.
    pub async fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.wake.notify_one();
        let handle = self
            .handle
            .lock()
            .expect("the worker-handle lock should not be poisoned")
            .take();
        if let Some(handle) = handle {
            if tokio::time::timeout(SHUTDOWN_GRACE, handle).await.is_err() {
                eprintln!("(joy) memory consolidation was still running; shutting it down — the unfinished batch will be claimed again at the next start");
            }
        }
    }
}

/// One pass: distill if due, then mirror if anything changed.
async fn run_once(deps: &Deps, pending: &AtomicI32, last_facts: &mut i64) -> anyhow::Result<()> {
    let resolved = {
        let guard = deps
            .resolved
            .read()
            .expect("the resolved lock should not be poisoned");
        guard.clone()
    };
    // No model configured (yet): nothing can be distilled, and saying so every minute
    // would be noise — the CLI already reported "模型还没配好" at startup.
    let Some(resolved) = resolved else {
        return Ok(());
    };
    let settings = {
        let guard = deps
            .settings
            .read()
            .expect("the settings lock should not be poisoned");
        guard.clone()
    };

    // Same rule as before, just off the turn's clock: enough new rows, then a small-model
    // call. The claim inside is what keeps two processes from doing it twice.
    let new_facts = joyczl_memory::consolidation::consolidate_if_due(
        &deps.chat,
        &deps.facts,
        &deps.episodes,
        resolved.client.as_ref(),
        &resolved.small_model,
        settings.consolidate_every,
    )
    .await?;
    if new_facts > 0 {
        pending.fetch_add(new_facts as i32, Ordering::SeqCst);
        eprintln!(
            "(joy) memory consolidation: {new_facts} new facts (the next turn will mention them)"
        );
    }

    // Mirror only when the store changed: rewriting identical pages every minute would
    // churn mtimes and make `git status` on a memory folder useless.
    let total = deps.facts.count().await.unwrap_or(-1);
    if total == *last_facts && new_facts == 0 {
        return Ok(());
    }
    *last_facts = total;

    let bundle = joyczl_memory::bundle::Bundle::new(&settings.home);
    match joyczl_memory::mirror::mirror_facts(&deps.facts, &bundle).await {
        Ok(report) if report.written > 0 || report.skipped > 0 => eprintln!(
            "(joy) memory library: wrote {} pages{}",
            report.written,
            if report.skipped > 0 {
                format!(
                    ", {} pages were edited by hand and left alone",
                    report.skipped
                )
            } else {
                String::new()
            }
        ),
        Ok(_) => {}
        Err(error) => {
            eprintln!("(joy) memory-library mirror failed (the facts themselves are fine): {error}")
        }
    }
    if let Err(error) =
        joyczl_memory::export_markdown(&deps.facts, &deps.episodes, &settings.home).await
    {
        eprintln!("(joy) MEMORY.md mirror failed (the memory itself is unaffected): {error}");
    }
    Ok(())
}

#[cfg(test)]
#[path = "memory_worker_tests.rs"]
mod memory_worker_tests;
