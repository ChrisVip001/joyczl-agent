//! The host implementation of background jobs: process + bounded ring + completion persistence.
//!
//! The tool layer only knows the `JobRegistry` trait (see `joyczl-tools/src/jobs.rs`); the real thing
//! lives here — the same injection discipline as subagent and approval: dependencies flow only from app-server toward tools.
//!
//! * **The same gate as the foreground**: starting a job first passes `pass_gate` (done by
//!   `run_command`'s handler), using the same `ExecPolicy` — background is not a side door around approval.
//! * **The ring is bounded**: on overflow it drops the oldest bytes and records how much was dropped,
//!   reporting it honestly on read. A screen-flooding command must not eat all memory, nor turn "read once" into a failure.
//! * **Output spills as it is written**: every output is also appended into `<home>/jobs/<pid>-<id>.log`
//!   (capped at 1 MiB; when exceeded, say so). The memory ring handles "read while alive"; this file handles "read after death".
//! * **The ledger goes to the db**: both start and finish write the jobs table in state.db (migration 0009).
//!   If the process dies, "what ran and how far" remains; startup reconciliation marks hanging jobs as
//!   interrupted — **never auto-rerun** (rerunning is an incident amplifier).

use std::collections::HashMap;

#[path = "jobs_buffer.rs"]
mod buffer;
use buffer::Log;
pub(crate) use buffer::Ring;
use std::sync::{Arc, Mutex};

use joyczl_tools::exec::ExecPolicy;
use joyczl_tools::jobs::{JobFut, JobId, JobRead, JobRegistry, JobSummary};
use tokio::io::AsyncReadExt;
use tokio::sync::oneshot;

/// Capacity of the output ring (bytes). 64 KiB is enough to watch progress without reaching memory.
const RING_BYTES: usize = 64 * 1024;
/// Longest wait for `wait=true`: shorter than a tool call should take; if the wait fails, say honestly it is still running.
const WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(20);
struct Status {
    running: bool,
    exit_code: Option<i32>,
}

struct Job {
    owner: String,
    command: String,
    ring: Arc<Mutex<Ring>>,
    status: Arc<Mutex<Status>>,
    /// Stop it: wake the task waiting on the child process so it can kill (the child belongs to that task).
    killer: Option<oneshot::Sender<()>>,
}

/// The in-process job table. **The ledger is in state.db** (see module header); memory holds only the live part.
pub struct Jobs {
    policy: ExecPolicy,
    home: std::path::PathBuf,
    jobs: Arc<Mutex<HashMap<JobId, Job>>>,
    rows: Arc<joyczl_state::Jobs>,
    /// Notifier (M3a): pushes job completions outward — a side path; absent means nothing is sent.
    notify: Option<Arc<dyn joyczl_tools::notify::Notifier>>,
    delivery_lock: Arc<tokio::sync::Mutex<()>>,
    /// In-memory snapshot of historical jobs (loaded at reconcile). Ledger queries are async while
    /// `JobRegistry::list` is sync — rather than building a sync bridge, load history once at startup:
    /// history never grows inside this process (newly finished jobs are already in the in-memory table).
    history: Arc<Mutex<Vec<joyczl_state::JobRow>>>,
}

impl Jobs {
    pub fn new(policy: ExecPolicy, home: std::path::PathBuf, rows: joyczl_state::Jobs) -> Self {
        Self {
            policy,
            home,
            jobs: Arc::new(Mutex::new(HashMap::new())),
            rows: Arc::new(rows),
            history: Arc::new(Mutex::new(Vec::new())),
            notify: None,
            delivery_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Attach the notifier (called at boot; not attached in tests).
    pub fn with_notifier(
        mut self,
        notify: Option<Arc<dyn joyczl_tools::notify::Notifier>>,
    ) -> Self {
        self.notify = notify;
        if let Some(notifier) = &self.notify {
            crate::job_delivery::start(&self.rows, notifier, &self.delivery_lock);
        }
        self
    }

    /// Startup reconciliation: jobs still running at the last crash are marked interrupted. **Never auto-rerun** —
    /// the rerun decision is left to the human or model looking at the ledger; this log line is their "what happened".
    pub async fn reconcile(&self) {
        match self.rows.reconcile_interrupted().await {
            Ok(rows) if rows.is_empty() => {}
            Ok(rows) => {
                // Take the snapshot **before** clearing: those rows are about to be marked interrupted,
                // but the history list needs them.
                *self
                    .history
                    .lock()
                    .expect("the history lock should not be poisoned") = rows.clone();
                for row in &rows {
                    eprintln!(
                        "(joy) job {} ({}) was still running at the last exit; marked interrupted. Output kept in {}. Rerunning it is your call.",
                        row.id,
                        row.command,
                        row.log_path.as_deref().unwrap_or("(no log)")
                    );
                }
            }
            Err(e) => {
                eprintln!("(joy) job ledger reconciliation failed (not blocking startup): {e}")
            }
        }
    }

    /// The one sentence for "not found", **shared by both paths**: read and stop must give the same
    /// answer — once the wording diverges, it leaks "this thing exists but is not yours".
    fn unknown(id: &str) -> String {
        format!("No job named {id} (job_list shows the ones that exist)")
    }

    /// Ledger lookback: the job is not in the in-memory table (process restarted / finished and cleared).
    /// The owner fence is still checked — history does not open up ownership just because it is "old".
    /// Output is read in full from the log file; historical jobs have no more increments, so it is given all at once.
    async fn read_from_ledger(
        rows: &Arc<joyczl_state::Jobs>,
        owner: &str,
        id: &str,
        _cursor: u64,
    ) -> Result<JobRead, String> {
        let row = rows
            .get(id)
            .await
            .map_err(|e| format!("the ledger could not be queried: {e}"))?
            .filter(|row| row.owner == owner)
            .ok_or_else(|| Self::unknown(id))?;
        let text = row
            .log_path
            .as_deref()
            .map(std::fs::read_to_string)
            .transpose()
            .ok()
            .flatten()
            .unwrap_or_default();
        Ok(JobRead {
            cursor: text.len() as u64,
            running: false,
            lossy: false,
            dropped: 0,
            exit_code: row.exit_code,
            text,
        })
    }

    /// Find a job and confirm it belongs to this session. Guessing someone else's id is useless.
    fn owned<'a>(
        jobs: &'a mut HashMap<JobId, Job>,
        owner: &str,
        id: &str,
    ) -> Result<&'a mut Job, String> {
        let job = jobs.get_mut(id).ok_or_else(|| Self::unknown(id))?;
        if job.owner != owner {
            // Do not reveal "this thing exists but is not yours": only say not found.
            return Err(Self::unknown(id));
        }
        Ok(job)
    }
}

impl JobRegistry for Jobs {
    fn spawn(
        &self,
        owner: &str,
        command: &str,
        cwd: Option<String>,
    ) -> JobFut<Result<JobId, String>> {
        self.spawn_with_run(owner, command, cwd, None)
    }
    fn spawn_for_run(
        &self,
        owner: &str,
        command: &str,
        cwd: Option<String>,
        run_id: Option<String>,
    ) -> JobFut<Result<JobId, String>> {
        self.spawn_with_run(owner, command, cwd, run_id)
    }
    fn output(
        &self,
        owner: &str,
        id: &JobId,
        cursor: u64,
        wait: bool,
    ) -> JobFut<Result<JobRead, String>> {
        let owner = owner.to_string();
        let id = id.clone();
        let table = self.jobs.clone();
        let rows = self.rows.clone();
        Box::pin(async move {
            // Take out the thing to wait on first, **never holding the lock across an await**.
            let table = table.clone();
            // Not in the live table → look back in the ledger. The guard must be released before the await
            // (a std MutexGuard is not Send; holding it across an await makes the whole future not Send).
            let found = {
                let jobs = table.lock().expect("the jobs lock should not be poisoned");
                jobs.get(&id)
                    .filter(|job| job.owner == owner)
                    .map(|job| (job.ring.clone(), job.status.clone()))
            };
            let (ring, status) = match found {
                Some(handles) => handles,
                None => return Self::read_from_ledger(&rows, &owner, &id, cursor).await,
            };

            if wait {
                let deadline = std::time::Instant::now() + WAIT_LIMIT;
                loop {
                    let running = status
                        .lock()
                        .expect("the status lock should not be poisoned")
                        .running;
                    if !running || std::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }

            let running = status
                .lock()
                .expect("the status lock should not be poisoned")
                .running;
            let exit_code = status
                .lock()
                .expect("the status lock should not be poisoned")
                .exit_code;
            let (text, cursor, lossy, dropped) = {
                let ring = ring.lock().expect("the ring lock should not be poisoned");
                let (text, cursor, lossy) = ring.read_from(cursor);
                (text, cursor, lossy, ring.dropped)
            };
            Ok(JobRead {
                text,
                cursor,
                running,
                lossy,
                dropped,
                exit_code,
            })
        })
    }

    fn kill(&self, owner: &str, id: &JobId) -> JobFut<Result<bool, String>> {
        let owner = owner.to_string();
        let id = id.clone();
        let table = self.jobs.clone();
        Box::pin(async move {
            let mut jobs = table.lock().expect("the jobs lock should not be poisoned");
            let job = Self::owned(&mut jobs, &owner, &id)?;
            let running = job
                .status
                .lock()
                .expect("the status lock should not be poisoned")
                .running;
            if !running {
                return Ok(false);
            }
            if let Some(killer) = job.killer.take() {
                let _ = killer.send(());
            }
            // Mark "stopped" immediately: waiting for that task to actually reap the process takes a
            // while, and the user is asking "is it stopped" right now.
            job.status
                .lock()
                .expect("the status lock should not be poisoned")
                .running = false;
            Ok(true)
        })
    }

    fn list(&self, owner: &str) -> Vec<JobSummary> {
        let jobs = self
            .jobs
            .lock()
            .expect("the jobs lock should not be poisoned");
        let mut out: Vec<JobSummary> = jobs
            .iter()
            .filter(|(_, job)| job.owner == owner)
            .map(|(id, job)| {
                let status = job
                    .status
                    .lock()
                    .expect("the status lock should not be poisoned");
                JobSummary {
                    id: id.clone(),
                    command: job.command.clone(),
                    running: status.running,
                    exit_code: status.exit_code,
                    bytes: job
                        .ring
                        .lock()
                        .expect("the ring lock should not be poisoned")
                        .total(),
                }
            })
            .collect();
        drop(jobs);
        // History from the ledger (the ones from before the process restart) is listed too — "the one that
        // got cut off halfway last time" must be visible, otherwise interrupted is just a tombstone nobody
        // knows about. History comes from the snapshot loaded at startup, purely synchronous, no blocking queries.
        for row in self
            .history
            .lock()
            .expect("the history lock should not be poisoned")
            .iter()
        {
            if row.owner != owner || out.iter().any(|summary| summary.id == row.id) {
                continue;
            }
            let bytes = row
                .log_path
                .as_deref()
                .and_then(|path| std::fs::metadata(path).ok())
                .map(|meta| meta.len())
                .unwrap_or(0);
            out.push(JobSummary {
                id: row.id.clone(),
                command: row.command.clone(),
                running: false,
                exit_code: row.exit_code,
                bytes,
            });
        }
        // Stable display order; random durable identities do not imply start order.
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
}

impl Jobs {
    fn spawn_with_run(
        &self,
        owner: &str,
        command: &str,
        cwd: Option<String>,
        run_id: Option<String>,
    ) -> JobFut<Result<JobId, String>> {
        let owner = owner.to_string();
        let command = command.to_string();
        let policy = self.policy.clone();
        let home = self.home.clone();
        let table = self.jobs.clone();
        let rows = self.rows.clone();
        // A reference to self must not enter the async block (the lifetime does not live that long) — clone outside the block.
        let notifier = self.notify.clone();
        let delivery_lock = self.delivery_lock.clone();

        // Writable roots: the explicitly given working directory (or the process cwd) — exactly
        // the same sources as foreground `run_command` (plus the policy's extra_roots, merged
        // and filtered by `spawn_sandboxed`). Home is not a root any more: it is the control
        // plane, and "background it" must not be a side door into it.
        let mut writable: Vec<std::path::PathBuf> = Vec::new();
        if let Some(cwd) = cwd
            .map(std::path::PathBuf::from)
            .filter(|path| path.is_dir())
            .or_else(|| std::env::current_dir().ok())
        {
            writable.push(cwd);
        }

        Box::pin(async move {
            let job_id = rows
                .new_id()
                .await
                .map_err(|e| format!("Cannot allocate job: {e}"))?;

            let ring = Arc::new(Mutex::new(Ring::new(RING_BYTES)));
            let status = Arc::new(Mutex::new(Status {
                running: true,
                exit_code: None,
            }));

            // Output logs survive process death; the random job ID stays unique across restarts.
            let log_dir = home.join("jobs");
            let _ = std::fs::create_dir_all(&log_dir);
            let log_path = log_dir.join(format!("{}-{job_id}.log", std::process::id()));
            let log = Arc::new(Mutex::new(Log::open(&log_path)));
            rows.record_started_with_delivery(
                &job_id,
                &owner,
                &command,
                &log_path.display().to_string(),
                run_id.as_deref(),
                notifier.is_some(),
                notifier
                    .as_ref()
                    .and_then(|n| n.delivery_target())
                    .as_deref(),
            )
            .await
            .map_err(|e| format!("Cannot persist job intent: {e}"))?;
            let mut child = match joyczl_tools::exec::spawn_sandboxed(&command, &policy, &writable)
            {
                Ok(child) => child,
                Err(why) => {
                    if let Err(e) = rows.record_finished(&job_id, "failed", None).await {
                        eprintln!("(joy) Failed job {job_id} could not be recorded: {e}");
                    }
                    return Err(why);
                }
            };

            // Both read pipes drain concurrently: reading stdout fully before stderr would deadlock when
            // stderr fills up (the same reason as on the foreground path).
            let mut drains = Vec::new();
            if let Some(mut stdout) = child.stdout.take() {
                let ring = ring.clone();
                let log = log.clone();
                drains.push(tokio::spawn(async move {
                    let mut chunk = [0u8; 8192];
                    while let Ok(read) = stdout.read(&mut chunk).await {
                        if read == 0 {
                            break;
                        }
                        ring.lock()
                            .expect("the ring lock should not be poisoned")
                            .push(&chunk[..read]);
                        log.lock()
                            .expect("the log lock should not be poisoned")
                            .push(&chunk[..read]);
                    }
                }));
            }
            if let Some(mut stderr) = child.stderr.take() {
                let ring = ring.clone();
                let log = log.clone();
                drains.push(tokio::spawn(async move {
                    let mut chunk = [0u8; 8192];
                    while let Ok(read) = stderr.read(&mut chunk).await {
                        if read == 0 {
                            break;
                        }
                        ring.lock()
                            .expect("the ring lock should not be poisoned")
                            .push(&chunk[..read]);
                        log.lock()
                            .expect("the log lock should not be poisoned")
                            .push(&chunk[..read]);
                    }
                }));
            }

            let (killer, killed) = oneshot::channel::<()>();
            let watched_ring = ring.clone();
            let watched_status = status.clone();
            let watched_command = command.clone();
            let watched_home = home.clone();
            let watched_id = job_id.clone();
            let watched_rows = rows.clone();
            tokio::spawn(async move {
                // Wait for it to finish by itself, or until "someone wants it stopped". A stopped job is
                // recorded as killed — "failed" and "stopped by someone" are two different things in the ledger.
                let (exit, was_killed) = tokio::select! {
                    exit = child.wait() => (exit, false),
                    _ = killed => {
                        let _ = child.kill().await;
                        (child.wait().await, true)
                    }
                };
                let code = exit.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
                // Process exit can precede EOF on both pipes. Publish completion only after
                // draining and persisting output; inherited pipes must not hold us forever.
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
                for mut drain in drains {
                    match tokio::time::timeout_at(deadline, &mut drain).await {
                        Ok(Ok(())) => {}
                        outcome => {
                            if outcome.is_err() {
                                drain.abort();
                                let _ = drain.await;
                            }
                            let note = format!("\n[output drain incomplete: {outcome:?}]\n");
                            watched_ring
                                .lock()
                                .expect("ring lock")
                                .push(note.as_bytes());
                            log.lock().expect("log lock").push(note.as_bytes());
                            eprintln!("(joy) job {watched_id}: output drain incomplete");
                        }
                    }
                }
                let db_status = if was_killed {
                    "killed"
                } else if code == 0 {
                    "completed"
                } else {
                    "failed"
                };
                if let Err(e) = watched_rows
                    .record_finished(&watched_id, db_status, Some(code))
                    .await
                {
                    eprintln!(
                        "(joy) the closing of job {watched_id} was not written to the ledger: {e}"
                    );
                }
                if let Some(notifier) = &notifier {
                    let _delivery = delivery_lock.lock().await;
                    crate::job_delivery::deliver(&watched_rows, notifier.as_ref()).await;
                }
                // Completion spill: still queryable after the process exits (not "keeps running across restarts").
                let bytes = watched_ring
                    .lock()
                    .expect("the ring lock should not be poisoned")
                    .snapshot();
                let dir = watched_home.join("outbox").join("jobs");
                if std::fs::create_dir_all(&dir).is_ok() {
                    let body = format!(
                        "# {watched_id}: {watched_command}\nexit code {code}\n\n{}",
                        String::from_utf8_lossy(&bytes)
                    );
                    // Keep the existing filename shape for readers of historical output.
                    let file = dir.join(format!("{}-{watched_id}.txt", std::process::id()));
                    if let Err(e) = std::fs::write(&file, body) {
                        eprintln!("(joy) the output of background job {watched_id} was not written to a file: {e}");
                    }
                }
                {
                    let mut status = watched_status.lock().expect("status lock");
                    status.exit_code = Some(code);
                    status.running = false;
                }
                eprintln!("(joy) background job {watched_id} finished (exit code {code})");
            });

            table
                .lock()
                .expect("the jobs lock should not be poisoned")
                .insert(
                    job_id.clone(),
                    Job {
                        owner,
                        command,
                        ring,
                        status,
                        killer: Some(killer),
                    },
                );
            Ok(job_id)
        })
    }
}
