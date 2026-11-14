//! `joyczl-app-server` — the JSON-RPC server, the process where Joy's brain lives.
//!
//! The fulcrum of the whole three-language architecture: **only this process opens state.db**.
//! The CLI, dashboard, chat gateway, and Python SDK are all its clients, speaking newline-delimited
//! JSON-RPC over stdio. Therefore:
//!
//!   * the problem of sharing a SQLite connection across threads does not exist (no sharing needed at all);
//!   * async exists in exactly one place, the server (the gateway can stay TypeScript, using the existing ecosystem);
//!   * state is single: clients in any language all see the same memory.
//!
//! P2b added streaming: every time the model emits a chunk of text, the client immediately receives a `textDelta`.
//! All outbound output (notifications and responses) converges into one channel, written to stdout by the
//! single writer — order is thereby guaranteed, and no concurrent write can corrupt a line.
//!
//! It can start even without a configured key: memory and session methods work as usual, only `turn/*`
//! reports PROVIDER_ERROR, telling the caller where to get a key.

mod agents_md;
mod approval;
mod completion_delivery;
mod dispatch;
mod goal;
mod image_meta;
mod inbox;
mod input_wake;
mod job_delivery;
mod jobs;
mod ledger_writer;
/// Guardian lock: only one app-server per home (`joyczl_app_server::lock`).
pub mod lock;
pub mod memory_center;
#[cfg(test)]
mod memory_center_tests;
mod memory_worker;
pub mod models;
mod resources;
mod resume;
mod route;
mod schedule_output;
mod stdio;
mod subagent;
mod task;
mod task_contract;
mod task_dispatch;
mod task_handoff;
mod task_inputs;
mod task_operations;
mod task_recovery;
mod task_waits;
mod trace;
mod turn;
mod turn_failure;
mod turn_operations;
mod wake;

#[cfg(test)]
mod turn_tests;

#[cfg(test)]
#[path = "resume_tests.rs"]
mod resume_tests;

#[cfg(test)]
#[path = "approval_tests.rs"]
mod approval_tests;

#[cfg(test)]
#[path = "todo_tests.rs"]
mod todo_tests;

#[cfg(test)]
#[path = "jobs_tests.rs"]
mod jobs_tests;

#[cfg(test)]
#[path = "lock_tests.rs"]
mod lock_tests;

#[cfg(test)]
#[path = "goal_tests.rs"]
mod goal_tests;

#[cfg(test)]
#[path = "task_tests.rs"]
mod task_tests;

#[cfg(test)]
#[path = "inbox_tests.rs"]
mod inbox_tests;

#[cfg(test)]
#[path = "image_meta_tests.rs"]
mod image_meta_tests;

#[cfg(test)]
#[path = "subagent_tests.rs"]
mod subagent_tests;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use joyczl_config::Settings;
use joyczl_loop::{Interrupt, Steering};
use joyczl_protocol::{
    JsonRpcMessage, RememberScope, ServerNotification, SettingsPatch, SettingsView,
};
use joyczl_provider::Resolved;
use joyczl_state::{Calendar, Chat, Episodes, Facts};
use joyczl_tools::approval::RememberTarget;
use joyczl_tools::ToolCtx;
use joyczl_tools::ToolRegistry;
use sqlx::sqlite::SqlitePool;
use tokio::sync::mpsc;
use tokio::sync::Mutex as AsyncMutex;

pub use dispatch::handle;
pub use stdio::run_stdio;
pub use turn::run_turn;

/// One frame from server → client. Notifications and responses share the same channel — order is thereby guaranteed.
pub enum Frame {
    Notification(ServerNotification),
    Response(JsonRpcMessage),
}

/// The outlet for pushing things to the client. The turn execution holds it and sends text deltas
/// as soon as they arrive, instead of waiting for the whole turn to finish.
#[derive(Clone)]
pub struct EventSink {
    tx: mpsc::UnboundedSender<Frame>,
}

impl EventSink {
    pub(crate) fn new(tx: mpsc::UnboundedSender<Frame>) -> Self {
        Self { tx }
    }

    /// Outlet for in-process clients (terminal REPL): it starts its own channel, and notifications and
    /// responses are taken frame by frame from the receiver. The same order guarantee as stdio.
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<Frame>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }

    /// A send failure has only one possible cause: the writer is already gone (the process is exiting).
    /// Worrying about that notification then is pointless, so stay silent.
    pub(crate) fn notification(&self, notification: ServerNotification) {
        let _ = self.tx.send(Frame::Notification(notification));
    }

    pub(crate) fn response(&self, response: JsonRpcMessage) {
        let _ = self.tx.send(Frame::Response(response));
    }
}

/// Outcome of `turn/follow-up`: queued (with its position), nothing to wait for, or a storage
/// failure. Three different things to tell a person; one enum instead of a bare `Result`.
pub(crate) enum FollowUp {
    Queued(i32),
    Idle,
    Failed(String),
}

/// A turn that is in flight: the cancellation token plus the two facts `turn/list` answers with.
/// One record instead of a second side table — two maps keyed by turn_id would eventually disagree.
pub(crate) struct ActiveTurn {
    pub(crate) interrupt: Arc<Interrupt>,
    pub(crate) session_id: String,
    /// RFC 3339 with offset, captured at registration.
    pub(crate) started_at: String,
    /// Sentences added while this turn runs (`turn/steer`). The loop folds them in at the next
    /// iteration boundary; the transcript reads them all afterwards.
    pub(crate) steering: Arc<Mutex<Steering>>,
}

/// Everything the server holds.
///
/// `resolved` is an `Option`: it must be able to start without a configured key. Memory and sessions
/// do not depend on the model; only `turn/*` needs it — and that is when "missing key" gets spelled out.
///
/// `settings` / `resolved` sit in `RwLock`s: `config/write` changes them while running, and the next
/// turn after the change uses the new values — no process restart needed. The lock only guards the few
/// microseconds of read-modify-write; there is no await inside.
///
/// `turns` is the cancellation-token table of running turns: `turn/interrupt` finds one by turn_id
/// and flips the switch. A finished turn (success or failure) is removed. Each entry also carries
/// the session it belongs to, because `turn/list` is asked **by session** — a table keyed only by
/// turn_id cannot answer "is this conversation running right now".
///
/// `Clone`: every field is a handle (Arc / connection pool / config); copying one does not copy state.
/// The full_agent node in the graph is a 'static closure and can only hold what it owns —
/// what it holds is exactly this clone.
#[derive(Clone)]
pub struct Server {
    pub(crate) facts: Facts,
    pub(crate) episodes: Episodes,
    pub(crate) chat: Chat,
    /// The append-only turn ledger (migration 0020): the record `chat_log` is a view of.
    /// Written at finalize time; read back by the memory worker and the memory center.
    pub(crate) ledger: joyczl_state::Ledger,
    /// The background memory worker (`open()` attaches it; tests and embedders may not).
    /// Its absence is not a degradation: `finalize_turn` falls back to doing the same
    /// work inline, so a library user who never starts a runtime still gets memory.
    pub(crate) memory_worker: Option<Arc<memory_worker::MemoryWorker>>,
    /// Usage ledger (migration 0010): bookkeeping only, never enforced.
    pub(crate) usage: joyczl_state::Usage,
    /// Turn checkpoints (migration 0013): a running turn's working memory, durable at each
    /// tool-results boundary — the fresh process resumes instead of replaying finished tools.
    pub(crate) checkpoints: joyczl_state::Checkpoints,
    /// Notifier (M3a): a side path; None when unconfigured.
    pub(crate) notifier: Option<Arc<dyn joyczl_tools::notify::Notifier>>,
    /// Approval audit ledger (M3b): who asked, and how it turned out.
    pub(crate) approval_log: joyczl_state::Approvals,
    pub(crate) calendar: Calendar,
    pub(crate) settings: Arc<RwLock<Settings>>,
    pub(crate) resolved: Arc<RwLock<Option<Resolved>>>,
    pub(crate) tools: Arc<ToolRegistry>,
    pub(crate) turns: Arc<Mutex<HashMap<String, ActiveTurn>>>,
    /// Approval requests waiting for a human: `approval/respond` finds one by turn_id + request_id
    /// and delivers the answer back. Same lifecycle discipline as `turns` (the whole bucket is dropped when a turn ends).
    pub(crate) approvals: approval::Pending,
    /// Commands a human answered "always allow in this session" to, live for this process: the very same cell the
    /// exec tool reads (`ExecPolicy::session_allow`), so the next call sees the yes immediately —
    /// not after a restart. Never persisted (that is `settings.json`'s `execAllow`).
    pub(crate) exec_session_allow: Arc<Mutex<Vec<String>>>,
    /// Directories a write was approved for ("always allow in this session" / "remember this one"), same idea: the cell is
    /// shared with `WritePolicy::approved_dirs`, so the next write skips the question. The
    /// persisted half comes from `settings.json`'s `writeAllowDirs` at startup.
    pub(crate) write_approved_dirs: Arc<Mutex<Vec<PathBuf>>>,
    /// Lifecycle hooks (`<home>/hooks.json`). `None` = unconfigured or disabled.
    pub(crate) hooks: Option<Arc<joyczl_tools::hooks::Hooks>>,
    /// Session -> todo list (see todo.rs in joyczl-tools). Injected into the system prompt every turn,
    /// so it does not depend on history and compaction cannot wash it away.
    pub(crate) todo: Arc<joyczl_tools::todo::TodoBoard>,
    /// Background job table (switched on/off together with `JOY_EXEC`).
    pub(crate) jobs: Option<Arc<dyn joyczl_tools::jobs::JobRegistry>>,
    /// Connector inbox (migration 0015): what came in from the outside, recorded as-is.
    /// Only the poller and `inbox/*` touch it; the model has no path here at all.
    pub(crate) inbox: joyczl_state::Inbox,
    /// Long tasks (migration 0014, rebuilt in 0017): identity, status, acceptance, budget,
    /// spent. Human-only switches live in task.rs; the goal loop is the engine that drives rounds.
    pub(crate) tasks: joyczl_state::Tasks,
    pub(crate) completions: Arc<joyczl_state::completions::Completions>,
    pub(crate) completion_lock: Arc<AsyncMutex<()>>,
    /// One row per execution of a task (migration 0017): "executed N times", what each cost.
    pub(crate) runs: joyczl_state::Runs,
    /// One row per tool call (migration 0017) — where idempotency lives: a step left `running`
    /// is never replayed, it waits for a human.
    pub(crate) steps: joyczl_state::Steps,
    /// The task timeline: verification rounds, budget refusals, contradictions.
    pub(crate) events: joyczl_state::Events,
    /// The deliverable list: files that really exist after a round.
    pub(crate) artifacts: joyczl_state::Artifacts,
    /// Messages queued to run **after** the turn in flight (0.16 follow-up, migration 0019).
    pub(crate) queue: joyczl_state::Queue,
    /// Turns a human just asked to suspend (`task/suspend`): at finalize time the checkpoint is
    /// kept as `suspended` instead of `abandoned` — pausing is not stopping.
    pub(crate) suspending: Arc<Mutex<HashSet<String>>>,
    /// Multiple model profiles (`<home>/models.json`) and the credentials backend (see models.rs).
    /// Switching models = edit here + re-resolve the provider, no restart.
    pub(crate) models: models::Models,
    pub(crate) secrets: models::Secrets,
    /// Models resolved per tier (route.rs): each profile is resolved once — the provider
    /// client holds the connection pool. When a profile is edited/deleted, `reapply` invalidates the whole table.
    pub(crate) route_cache: Arc<std::sync::Mutex<std::collections::HashMap<String, Resolved>>>,
    /// The merged model catalog (built-in table + `<home>/models-catalog.json`), loaded
    /// once at boot. `model/save` validates against it; `model/list` shows it.
    pub(crate) catalog: joyczl_provider::catalog::Catalog,
    /// One **write-section lock** per session: the serialisation point for a turn's
    /// write sections (history append / consolidation / compaction). Model-call
    /// sections never hold it — that would stall the session and keep interrupt out.
    /// Lazily created and **never removed**: removal would race two concurrent turns
    /// into holding two different locks.
    pub(crate) session_locks: Arc<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
    _execution_owner: Option<Arc<lock::DaemonLock>>,
    pub(crate) pool: SqlitePool,
    /// The last turn's context breakdown per session — the memory center's meter.
    ///
    /// Written at assembly time (that is the only moment every section is in hand), read by
    /// `context/usage`. Absent = no turn has run yet, which the meter shows as "not measured"
    /// rather than a fake zero.
    pub(crate) context_usage: Arc<Mutex<HashMap<String, joyczl_protocol::ContextUsage>>>,
    /// Deferred-tool activations, per session (`JOY_TOOL_POLICY=deferred`).
    ///
    /// A session-scoped cell, not a per-turn one: `find_tools` runs in one turn and its
    /// result is read in the next, so a fresh set every turn would make the model rediscover
    /// the same tool on every message. In memory on purpose — a restarted server asks the
    /// model to search again, which is honest and cheap; a stale table on disk is neither.
    pub(crate) activations: Arc<Mutex<HashMap<String, joyczl_tools::ActivationSet>>>,
}

impl Server {
    /// Start the background memory worker on this server (idempotent).
    ///
    /// **The process owns the worker, not the library**: the CLI (REPL and the stdio
    /// daemon) calls this; `open()` does not, so the eval harness and assembly tests stay
    /// deterministic (a background model call must never race an assertion). Without a
    /// worker, `finalize_turn` does the same memory work inline — one function, two callers.
    pub fn attach_memory_worker(&mut self) {
        if self.memory_worker.is_some() {
            return;
        }
        let worker = memory_worker::MemoryWorker::spawn(memory_worker::Deps {
            chat: self.chat.clone(),
            facts: self.facts.clone(),
            episodes: self.episodes.clone(),
            settings: self.settings.clone(),
            resolved: self.resolved.clone(),
            _execution_owner: self._execution_owner.clone(),
        });
        self.memory_worker = Some(worker);
    }

    /// Stop the worker, waiting briefly for a run in flight. Called on orderly shutdown.
    pub async fn stop_memory_worker(&self) {
        if let Some(worker) = self.memory_worker.as_ref() {
            worker.stop().await;
        }
    }

    /// Assembled from state.db. A provider-resolution failure is not fatal — record the reason and report it at turn time.
    /// Set or clear the goal on this session's task. **Only a human can reach here** — the model
    /// has no task tool, and the REPL's `/goal` is the only caller.
    ///
    /// A goal is a column on the row (`agent_tasks.goal_condition`), not a record of its own, so
    /// "set a goal" is either "create that task" or "edit it". Clearing keeps the row and its
    /// title: the task survives as a single-turn one. Returns the row as it now reads, so the
    /// caller prints what the ledger says instead of what it hoped.
    pub async fn set_task_goal(
        &self,
        session_id: &str,
        condition: Option<&str>,
    ) -> Option<joyczl_state::TaskRow> {
        let trimmed = condition.map(str::trim).filter(|text| !text.is_empty());
        let active = self.tasks.active(session_id).await.ok().flatten();
        match (trimmed, active) {
            // No task yet: the goal **is** what creates one — the same row the card and the loop read.
            (Some(condition), None) => {
                let max_rounds = self
                    .settings
                    .read()
                    .expect("the settings lock should not be poisoned")
                    .goal_max_rounds;
                let id = format!("task{}", chrono::Local::now().timestamp_millis());
                if let Err(e) = self
                    .tasks
                    .create(joyczl_state::NewTask {
                        id: &id,
                        session_id: Some(session_id),
                        title: condition,
                        goal_condition: Some(condition),
                        acceptance_json: "[]",
                        budget_json: "{}",
                        cwd: None,
                        max_rounds,
                        tier: None,
                    })
                    .await
                {
                    eprintln!("(joy) the goal could not be turned into a task: {e}");
                    return None;
                }
            }
            (Some(condition), Some(task)) => {
                if let Err(e) = self.tasks.set_goal(&task.id, Some(condition)).await {
                    eprintln!("(joy) the goal was not written to the task ledger: {e}");
                }
            }
            // Clearing: the task survives as a single-turn one (its title remains the record).
            (None, Some(task)) => {
                if let Err(e) = self.tasks.set_goal(&task.id, None).await {
                    eprintln!("(joy) clearing the goal was not written to the task ledger: {e}");
                }
            }
            (None, None) => return None,
        }
        self.tasks.active(session_id).await.ok().flatten()
    }

    /// The home this server was booted with (connectors.json, outbox — same file the
    /// ops side reads; one machine, one truth).
    pub(crate) fn home(&self) -> std::path::PathBuf {
        self.settings
            .read()
            .expect("the settings lock should not be poisoned")
            .home
            .clone()
    }

    /// Isolated embedding: the caller must exclusively own the supplied store.
    /// Production entry points use `open` to acquire ownership before recovery.
    pub async fn boot(pool: SqlitePool, settings: Settings) -> Self {
        Self::boot_owned(pool, settings, None).await
    }

    async fn boot_owned(
        pool: SqlitePool,
        mut settings: Settings,
        execution_owner: Option<Arc<lock::DaemonLock>>,
    ) -> Self {
        // The model profile picked in the UI is newer and speaks louder than env vars typed at startup.
        let secrets = models::new_secrets();
        models::apply_at_boot(&mut settings, &secrets);
        let models = models::new_models(joyczl_config::models::load_models(&settings.home));
        // When routing is on, make it visible: which file, which mode — step one of explaining "why this turn used that model".
        let routes_boot = joyczl_config::routes::load_routes(&settings.home);
        if routes_boot.mode == joyczl_config::routes::RouteMode::Auto {
            eprintln!(
                "(joy) model routing: auto ({} profiles, {} tier mappings)",
                joyczl_config::routes::routes_path(&settings.home).display(),
                routes_boot.routes.len()
            );
        }
        let route_cache = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let settings = Arc::new(RwLock::new(settings));
        // The read lock is taken only inside an expression: **never held across an await** (clippy would
        // stop it, and rightly so — a read lock stuck on an await would clog the whole server).
        let snapshot = settings
            .read()
            .expect("the settings lock should not be poisoned")
            .clone();
        // The merged model catalog: built-in table + the user's override file. Loaded once —
        // switching models re-resolves, it does not re-read the catalog.
        let catalog = joyczl_provider::catalog::Catalog::load(&snapshot.home);
        let resolved = match joyczl_provider::resolve_with(&snapshot, &catalog) {
            Ok(resolved) => Some(resolved),
            Err(reason) => {
                // Log to stderr: stdout is the protocol channel and must not mix in logs.
                eprintln!("(joy) no model configured yet, turn/* unavailable — {reason}");
                None
            }
        };
        let resolved = Arc::new(RwLock::new(resolved));
        // The tool registry must connect to MCP (if configured), so it is async — in assembly order it
        // has to be built first, then placed into the Server.
        // Missing skill dependencies are announced **once at startup** (not every turn): what they affect
        // is "can this skill still be triggered implicitly" — a load-time fact.
        {
            let mut loader = joyczl_memory::skills::SkillLoader::new(
                joyczl_memory::skills::SkillLoader::dirs_for(&snapshot.home),
            );
            for (skill, missing) in loader.dangling_dependencies() {
                eprintln!("(joy) skill '{skill}' depends on '{missing}', which is missing; it takes no part in implicit triggering");
            }
        }

        // Lifecycle hooks: **off by default** (hooks.json is read only with `JOY_HOOKS=1`). When enabled, print
        // one line saying how many rules run and from which file — hooking commands onto every tool call must be visible.
        let hooks = if snapshot.hooks_enabled {
            match joyczl_tools::hooks::Hooks::load(
                &snapshot.home,
                Some(std::time::Duration::from_secs(
                    snapshot.hooks_timeout_secs.max(1) as u64,
                )),
            ) {
                Some(hooks) => {
                    eprintln!("(joy) hooks enabled: {}", hooks.summary());
                    Some(Arc::new(hooks))
                }
                None => {
                    eprintln!(
                        "(joy) JOY_HOOKS=1 but {} does not exist or has no hooks — not enabling this time",
                        snapshot.home.join("hooks.json").display()
                    );
                    None
                }
            }
        } else {
            None
        };

        let notifier = joyczl_tools::notify::load(&snapshot.home, snapshot.notify_enabled);
        let approval_log = joyczl_state::Approvals::new(pool.clone());
        let builtin = builtin_tools(
            &snapshot,
            pool.clone(),
            notifier.clone(),
            execution_owner.clone(),
        )
        .await;
        let BuiltinTools {
            mut tools,
            jobs: jobs_handle,
            exec_session_allow,
            write_approved_dirs,
        } = builtin;
        tools.set_hooks(hooks.clone());
        tools.set_resources(Arc::new(resources::Mutations::default()));
        let facts = Facts::new(pool.clone());
        let episodes = Episodes::new(pool.clone());
        let chat = Chat::new(pool.clone());
        match chat.recover_consolidation_claims().await {
            Ok(count) if count > 0 => eprintln!("(joy) recovered {count} unfinished memory source rows; extraction will retry without replaying task tools"),
            Ok(_) => {}
            Err(error) => eprintln!("(joy) memory claim recovery failed; claims retained for the next startup: {error}"),
        }
        // One-shot: an existing home starts with an empty ledger. Fill it from the folded
        // view once, and say so — a memory center that shows nothing on day one looks
        // broken, and pretending old turns are rebuildable would be worse.
        let ledger = joyczl_state::Ledger::new(pool.clone());
        if let Ok(0) = ledger.count().await {
            match joyczl_state::ledger::backfill_from_chat(&pool).await {
                Ok(report) if report.turns > 0 => eprintln!(
                    "(joy) first start: backfilling {} turns from {} sessions into the ledger (marked folded — the tool output of those turns survives only as a summary)",
                    report.sessions, report.turns
                ),
                Ok(_) => {}
                Err(e) => eprintln!("(joy) ledger backfill failed (not blocking startup; run `joy memory backfill` later): {e}"),
            }
        }
        let usage = joyczl_state::Usage::new(pool.clone());
        let checkpoints = joyczl_state::Checkpoints::new(pool.clone());
        let calendar = Calendar::new(pool.clone());

        // Subagent: **off by default**, registered only when enabled. The runner gets "a copy of this
        // tool table" — **note `tools.clone()` is evaluated before register**, so the copy contains
        // no `delegate_task`: recursive delegation is thus not "denied", the option simply does not
        // exist. The reverse order would amount to enabling recursion.
        // Todo list: **present by default** (it touches no permissions, no storage, and blocks nothing;
        // just a sheet visible every turn). The subagent's table removes it — see subagent.rs.
        let todo = Arc::new(joyczl_tools::todo::TodoBoard::new());
        tools.register(joyczl_tools::todo::todo_write(todo.clone()));

        if snapshot.delegate_enabled {
            eprintln!(
                "(joy) subagents enabled: delegate_task is available (a subagent cannot delegate further and is not approval-gated)"
            );
            tools.register_exclusive(joyczl_tools::subagent::delegate_task(Arc::new(
                subagent::Delegated {
                    tools: Arc::new(tools.clone()),
                    facts: facts.clone(),
                    episodes: episodes.clone(),
                    chat: chat.clone(),
                    calendar: calendar.clone(),
                    // Shared handles, not snapshots: a turn arriving after a provider switch must use the new one.
                    settings: settings.clone(),
                    resolved: resolved.clone(),
                    hooks: hooks.clone(),
                },
            )));
        }

        let server = Self {
            inbox: joyczl_state::Inbox::new(pool.clone()),
            facts,
            episodes,
            chat,
            ledger,
            memory_worker: None,
            usage,
            checkpoints,
            notifier,
            approval_log,
            calendar,
            settings,
            resolved,
            tools: Arc::new(tools),
            turns: Arc::new(Mutex::new(HashMap::new())),
            approvals: Arc::new(Mutex::new(HashMap::new())),
            exec_session_allow,
            write_approved_dirs,
            hooks,
            todo,
            jobs: jobs_handle,
            queue: joyczl_state::Queue::new(pool.clone()),
            tasks: joyczl_state::Tasks::new(pool.clone()),
            completions: Arc::new(joyczl_state::completions::Completions::new(pool.clone())),
            completion_lock: Arc::new(AsyncMutex::new(())),
            runs: joyczl_state::Runs::new(pool.clone()),
            steps: joyczl_state::Steps::new(pool.clone()),
            events: joyczl_state::Events::new(pool.clone()),
            artifacts: joyczl_state::Artifacts::new(pool.clone()),
            suspending: Arc::new(Mutex::new(HashSet::new())),
            models,
            route_cache,
            catalog,
            session_locks: Arc::new(Mutex::new(HashMap::new())),
            secrets,
            pool,
            _execution_owner: execution_owner,
            context_usage: Arc::new(Mutex::new(HashMap::new())),
            activations: Arc::new(Mutex::new(HashMap::new())),
        };
        server.restore_tasks().await;
        crate::completion_delivery::start(
            &server.completions,
            server.notifier.as_ref(),
            &server.completion_lock,
            server.settings().home,
        );
        server
    }

    /// Flush already committed production receipts; this grants no execution capability.
    pub async fn flush_completion_notices(&self) {
        crate::completion_delivery::flush(self).await;
    }

    /// Boot restore for long tasks (migrations 0014 + 0017), in dependency order:
    ///
    /// 1. a step left `running` means "sent, its result never landed". It becomes `waiting` —
    ///    **never replayed**: a write that may already have happened is a human's call;
    /// 2. a run left `running` belonged to the dead process: closed as `waiting` too;
    /// 3. a `running` task row is marked suspended with the honest reason (the turn's `open`
    ///    checkpoint may then flip it back to running).
    ///
    /// There is no goal to restore any more: a goal is a column on the row, so step 3 already
    /// brings it back with everything else.
    async fn restore_tasks(&self) {
        // Ledger reconciliation is independent of whether execution is enabled this boot.
        if self.jobs.is_none() {
            match joyczl_state::Jobs::new(self.pool.clone())
                .reconcile_interrupted()
                .await
            {
                Ok(jobs) => {
                    for job in jobs {
                        eprintln!(
                            "(joy) job {} interrupted at restart; execution host is disabled",
                            job.id
                        );
                    }
                }
                Err(e) => eprintln!("(joy) disabled job host reconciliation failed: {e}"),
            }
        }

        match self
            .steps
            .settle_running("the process exited before this step was recorded, so the outcome is unknown — confirm whether to redo it")
            .await
        {
            Ok(0) => {}
            Ok(count) => eprintln!(
                "(joy) reconcile: {count} step(s) were interrupted before their result was recorded; marked waiting (not replayed — waiting on you)"
            ),
            Err(e) => eprintln!("(joy) step reconciliation could not read the database (not blocking startup): {e}"),
        }
        match self.runs.open_runs().await {
            Ok(runs) => {
                for run in runs {
                    eprintln!(
                        "(joy) reconcile: run {} (task {}) did not finish last time; marked waiting",
                        run.id, run.task_id
                    );
                    let _ = self
                        .runs
                        .close(
                            &run.id,
                            joyczl_state::RunStatus::Waiting,
                            Some("the process restarted before this run finished; waiting for confirmation"),
                        )
                        .await;
                }
            }
            Err(e) => eprintln!("(joy) run-ledger reconciliation could not read the database (not blocking startup): {e}"),
        }
        match self.tasks.reconcile_restart().await {
            Ok(rows) if !rows.is_empty() => {
                for row in &rows {
                    eprintln!(
                        "(joy) reconcile: task \"{}\" was still running when the process exited; marked suspended (session {})",
                        row.title,
                        row.session_id.as_deref().unwrap_or("(no session)")
                    );
                }
            }
            Ok(_) => {}
            Err(e) => eprintln!(
                "(joy) task reconciliation could not read the database (not blocking startup): {e}"
            ),
        }
        let dispatch = joyczl_state::dispatch::TaskDispatch::new(self.pool.clone());
        match dispatch.reconcile().await {
            Ok(ids) => {
                for id in ids {
                    eprintln!(
                        "(joy) task {id}: dispatch interrupted before checkpoint; needs review"
                    );
                }
            }
            Err(e) => eprintln!("(joy) task dispatch reconciliation failed: {e}"),
        }
        let (dispatch_sink, dispatch_rx) = EventSink::channel();
        drop(dispatch_rx);
        crate::task_dispatch::kick(self.clone(), dispatch_sink);
        match self.queue.reconcile_interrupted().await {
            Ok(rows) => {
                for row in rows {
                    let note = "(this queued instruction was interrupted; its outcome is unknown. Review the transcript and confirm before retrying.)";
                    eprintln!(
                        "(joy) queued instruction {} in session {} needs attention after restart",
                        row.id, row.session_id
                    );
                    if let Err(e) = self
                        .chat
                        .append_exchange_for(
                            &row.message,
                            note,
                            &row.session_id,
                            "app-server",
                            None,
                            None,
                        )
                        .await
                    {
                        eprintln!("(joy) interrupted queue notice could not be recorded: {e}");
                    }
                }
            }
            Err(e) => eprintln!("(joy) queued instruction reconciliation failed: {e}"),
        }
        match joyczl_state::waits::JobWaits::new(self.pool.clone())
            .pending()
            .await
        {
            Ok(waits) => {
                let (sink, receiver) = EventSink::channel();
                drop(receiver);
                for wait in waits {
                    eprintln!(
                        "(joy) restored background-job wait for task {}",
                        wait.task_id
                    );
                    crate::task_waits::arm(self.clone(), sink.clone(), wait);
                }
            }
            Err(e) => eprintln!("(joy) durable job waits could not be restored: {e}"),
        }
        match joyczl_state::external_waits::ExternalWaits::new(self.pool.clone())
            .pending()
            .await
        {
            Ok(waits) => {
                let (sink, receiver) = EventSink::channel();
                drop(receiver);
                for wait in waits {
                    eprintln!(
                        "(joy) restored external-input wait for task {}",
                        wait.task_id
                    );
                    crate::input_wake::arm(self.clone(), sink.clone(), wait);
                }
            }
            Err(error) => eprintln!("(joy) external-input waits could not be restored: {error}"),
        }
        // A queued follow-up is a promise made before the restart: "do this when you are done".
        // Handing it back to a turn is the difference between a queue and a note to self.
        match self.queue.sessions_with_pending().await {
            Ok(sessions) => {
                for session in sessions {
                    eprintln!("(joy) reconcile: session {session} still has queued messages; it carries on");
                    let server = self.clone();
                    tokio::spawn(async move { crate::turn::drain_queue(&server, &session).await });
                }
            }
            Err(e) => {
                eprintln!("(joy) queued messages could not be read (not blocking startup): {e}")
            }
        }
    }

    /// The session's write-section lock — created on first use, held forever.
    /// Two hash lookups are nothing next to what the lock actually protects.
    pub(crate) fn session_write_lock(&self, session_id: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self
            .session_locks
            .lock()
            .expect("the session_locks should not be poisoned");
        locks
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    /// The environment handed to handlers when executing tools. **Constructed in exactly one place**
    /// (shared by turn and subagent) — adding a field can never miss a path.
    ///
    /// `approval` is this turn's approval channel: the subagent passes `None` (it must not block on a human).
    pub(crate) fn tool_ctx(
        &self,
        session_id: &str,
        cwd: Option<std::path::PathBuf>,
        approval: Option<Arc<dyn joyczl_tools::approval::ApprovalBroker>>,
    ) -> ToolCtx {
        let settings = self.settings();
        tool_ctx(
            &self.facts,
            &self.episodes,
            &self.chat,
            &self.calendar,
            &settings.home,
            Injections {
                session_id: session_id.to_string(),
                cwd,
                approval,
                hooks: self.hooks.clone(),
                jobs: self.jobs.clone(),
            },
        )
    }

    /// Dashboard read surface: the session's todo sheet. `None` = never written.
    pub fn todo_view(&self, session_id: &str) -> Option<joyczl_protocol::TodoListView> {
        self.todo
            .read(session_id)
            .map(|list| joyczl_protocol::TodoListView {
                revision: list.revision as i64,
                items: list
                    .items
                    .into_iter()
                    .map(|item| joyczl_protocol::TodoItemView {
                        content: item.content,
                        status: item.status.as_str().to_string(),
                    })
                    .collect(),
            })
    }

    /// This turn's approval channel. `never` mode (the default) returns `None` — nobody asks, so nobody
    /// answers, and actions requiring approval are rejected outright.
    pub(crate) fn approval_bridge(
        &self,
        turn_id: &str,
        sink: &EventSink,
    ) -> Option<Arc<dyn joyczl_tools::approval::ApprovalBroker>> {
        let settings = self.settings();
        if settings.approval != "on-request" {
            return None;
        }
        Some(Arc::new(approval::Bridge {
            pending: self.approvals.clone(),
            sink: sink.clone(),
            turn_id: turn_id.to_string(),
            timeout_secs: settings.approval_timeout_secs,
            notify: self.notifier.clone(),
            logbook: self.approval_log.clone(),
        }))
    }

    /// Answer one approval request. Returns `false` = too late (timed out, or that turn already ended) —
    /// tell the client honestly, so it does not believe the approval took effect.
    ///
    /// `remember` says how long the yes lasts. It is applied **before** the answer is delivered, so the
    /// tool that asked (and the next call after it) already sees the new rule — an approval that only
    /// takes effect after a restart is not what the click promised.
    pub fn answer_approval(
        &self,
        turn_id: &str,
        request_id: &str,
        approved: bool,
        remember: RememberScope,
    ) -> bool {
        let waiting = self
            .approvals
            .lock()
            .expect("the approvals lock should not be poisoned")
            .get_mut(turn_id)
            .and_then(|per_turn| per_turn.remove(request_id));
        let Some(waiting) = waiting else {
            return false;
        };
        if approved {
            self.remember(waiting.remember.as_ref(), remember);
        }
        // Failing to deliver (the receiver is already gone) is not a failure: that turn has ended.
        let _ = waiting.tx.send(approved);
        true
    }

    /// Write the yes where it belongs, according to its scope.
    ///
    /// * `once` — nowhere: the next identical action asks again.
    /// * `session` — the live in-process cell only (never the disk).
    /// * `always` — the live cell **and** `settings.json`, so it survives a restart.
    ///
    /// A remembered rule is not a fence with a hole in it: `run_command` still meets hard-deny and the
    /// sandbox, and a write still has to be inside the writable boundary.
    fn remember(&self, target: Option<&RememberTarget>, scope: RememberScope) {
        let Some(target) = target else {
            return; // Nothing worth remembering (e.g. a one-off connector send)
        };
        if scope == RememberScope::Once {
            return;
        }
        match target {
            RememberTarget::Command(command) => {
                push_unique(&self.exec_session_allow, command.trim().to_string());
                if scope == RememberScope::Always {
                    self.persist_exec_allow(command.trim());
                }
            }
            RememberTarget::Directory(dir) => {
                push_unique(&self.write_approved_dirs, dir.clone());
                if scope == RememberScope::Always {
                    self.persist_write_dir(dir);
                }
            }
        }
    }

    /// "remember this one" for a command: replace the allow table wholesale and persist into `settings.json`.
    ///
    /// What is remembered is **this command itself**, no wildcards — what the user saw and approved was
    /// it, not the whole class. The live table was already updated by `remember`, so this only has to
    /// make it survive a restart.
    fn persist_exec_allow(&self, command: &str) {
        let mut settings = self.settings();
        if settings.exec_allow.iter().any(|rule| rule == command) {
            return; // Already on disk (probably asked twice in the same turn)
        }
        settings.exec_allow.push(command.to_string());
        let patch = SettingsPatch {
            exec_allow: Some(settings.exec_allow.clone()),
            ..Default::default()
        };
        match self.apply_config_patch(&patch) {
            Ok(_) => eprintln!("(joy) remembered this command: {command} (written to settings.json; effective now)"),
            Err(why) => eprintln!("(joy) could not remember this command: {why}"),
        }
    }

    /// "remember this one" for a write: remember the **directory**, never the file name.
    ///
    /// The directory is a "don't ask again" note, not a writable root — containment still decides
    /// whether a write may land there at all.
    fn persist_write_dir(&self, dir: &std::path::Path) {
        let mut settings = self.settings();
        let entry = dir.display().to_string();
        if settings
            .write_allow_dirs
            .iter()
            .any(|known| known.as_path() == dir)
        {
            return;
        }
        settings.write_allow_dirs.push(dir.to_path_buf());
        let patch = SettingsPatch {
            write_allow_dirs: Some(
                settings
                    .write_allow_dirs
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect(),
            ),
            ..Default::default()
        };
        match self.apply_config_patch(&patch) {
            Ok(_) => {
                eprintln!("(joy) remembered this directory: {entry} (writing here will not ask each time)")
            }
            Err(why) => eprintln!("(joy) could not remember this directory: {why}"),
        }
    }

    /// A snapshot of the current settings. The copy you get can be used for any length of time —
    /// concurrent writes only affect the next turn.
    /// The state-db connection pool (joy schedule's scheduling state goes into this same db — one dataset, no separate store). Clone is cheap.
    pub fn state_pool(&self) -> SqlitePool {
        self.pool.clone()
    }

    /// The notifier (M3a). Used by joy schedule for job-completion delivery; None when unconfigured.
    pub fn notifier(&self) -> Option<Arc<dyn joyczl_tools::notify::Notifier>> {
        self.notifier.clone()
    }

    pub fn settings(&self) -> Settings {
        self.settings
            .read()
            .expect("the settings lock should not be poisoned")
            .clone()
    }

    // ---- Read surface for in-process clients (terminal REPL) ------------------------------
    // The fields themselves are pub(crate): writes may only go through protocol methods; these
    // read-only handles serve local commands like /memory and /sessions.

    pub fn facts(&self) -> &Facts {
        &self.facts
    }

    pub fn episodes(&self) -> &Episodes {
        &self.episodes
    }

    pub fn chat(&self) -> &Chat {
        &self.chat
    }

    /// This session's deferred-tool activation set, minted on first use.
    ///
    /// Shared with the turn's tool view (`session_view_with`) so `find_tools` in one turn and
    /// `schemas()` in the next read the same cell.
    pub(crate) fn activation_set(&self, session_id: &str) -> joyczl_tools::ActivationSet {
        let mut sets = match self.activations.lock() {
            Ok(sets) => sets,
            Err(poisoned) => poisoned.into_inner(),
        };
        sets.entry(session_id.to_string())
            .or_insert_with(|| Arc::new(std::sync::Mutex::new(Default::default())))
            .clone()
    }

    /// A snapshot of the current provider resolution (None when no key is configured).
    pub fn resolved(&self) -> Option<Resolved> {
        self.resolved
            .read()
            .expect("the resolved lock should not be poisoned")
            .clone()
    }

    pub(crate) fn set_settings(&self, settings: Settings) {
        *self
            .settings
            .write()
            .expect("the settings lock should not be poisoned") = settings;
    }

    pub(crate) fn set_resolved(&self, resolved: Option<Resolved>) {
        *self
            .resolved
            .write()
            .expect("the resolved lock should not be poisoned") = resolved;
        if tokio::runtime::Handle::try_current().is_ok() {
            let (sink, receiver) = EventSink::channel();
            drop(receiver);
            crate::task_dispatch::kick(self.clone(), sink);
        }
    }

    /// Inject a provider resolution. The injection point for eval and tests: the normal path goes
    /// through `joyczl_provider::resolve` (env vars supply the key); eval needs a scripted model to
    /// pin harness behavior offline and deterministically — that is when one is plugged in from outside.
    pub fn install_provider(&self, resolved: Resolved) {
        self.set_resolved(Some(resolved));
    }

    /// Restrict an embedded host's tool surface. This can only remove capabilities.
    pub fn restrict_tools(&mut self, allowed: &[&str]) {
        let names: Vec<String> = self.tools.names().into_iter().map(str::to_string).collect();
        let mut tools = (*self.tools).clone();
        for name in names {
            if !allowed.contains(&name.as_str()) {
                tools = tools.without(&name);
            }
        }
        self.tools = Arc::new(tools);
    }

    /// Claim a session for one turn, handing out its cancellation token and steering channel.
    ///
    /// **One driver per session.** Two turns on one session would interleave their write
    /// sections and each would see half the other's history — before 0.16 nothing stopped it.
    /// `Err` carries the turn that holds the session: the only part of the answer the caller
    /// can act on ("wait for it, or steer it").
    pub(crate) fn begin_turn(
        &self,
        turn_id: &str,
        session_id: &str,
    ) -> Result<(Arc<Interrupt>, Arc<Mutex<Steering>>), String> {
        let mut turns = self
            .turns
            .lock()
            .expect("the turns lock should not be poisoned");
        if let Some((holder, _)) = turns.iter().find(|(_, turn)| turn.session_id == session_id) {
            return Err(holder.clone());
        }
        let interrupt = Interrupt::new();
        let steering = Steering::new();
        turns.insert(
            turn_id.to_string(),
            ActiveTurn {
                interrupt: interrupt.clone(),
                session_id: session_id.to_string(),
                started_at: chrono::Local::now().to_rfc3339(),
                steering: steering.clone(),
            },
        );
        Ok((interrupt, steering))
    }

    /// The steering channel of a running turn — what `turn/start` hands to the loop and what the
    /// transcript reads afterwards (a message accepted just before the end must still be logged).
    pub(crate) fn steering_of(&self, turn_id: &str) -> Option<Arc<Mutex<Steering>>> {
        self.turns
            .lock()
            .expect("the turns lock should not be poisoned")
            .get(turn_id)
            .map(|turn| turn.steering.clone())
    }

    /// Add one sentence to the turn running in this session.
    ///
    /// `Err` = nothing is running (the caller should `turn/start` instead — falling through to a
    /// new turn silently would be a lie about what happened). `Ok` carries the turn id and how
    /// many additions it has accepted, this one included.
    pub(crate) fn steer(&self, session_id: &str, message: &str) -> Result<(String, i32), ()> {
        let turns = self
            .turns
            .lock()
            .expect("the turns lock should not be poisoned");
        let Some((turn_id, turn)) = turns.iter().find(|(_, turn)| turn.session_id == session_id)
        else {
            return Err(());
        };
        let mut steering = turn
            .steering
            .lock()
            .expect("the steering lock should not be poisoned");
        steering.push(message);
        Ok((turn_id.clone(), steering.all().len() as i32))
    }

    /// Queue a message to run **after** the turn in flight (0.16 follow-up).
    ///
    /// Refused when nothing is running: "when you are done" with nothing to be done is just
    /// "now", and the caller should `turn/start` and watch its stream instead of queueing into
    /// an idle session that would never drain.
    pub(crate) async fn follow_up(&self, session_id: &str, message: &str) -> FollowUp {
        if self.active_turns(Some(session_id)).is_empty() {
            return FollowUp::Idle;
        }
        match self.queue.push(session_id, message).await {
            Ok(position) => FollowUp::Queued(position as i32),
            Err(e) => FollowUp::Failed(e.to_string()),
        }
    }

    /// The turns running right now (optionally for one session), oldest first — a list that
    /// reshuffles between two polls reads as "something changed" when nothing did.
    pub(crate) fn active_turns(&self, session: Option<&str>) -> Vec<(String, String, String)> {
        let turns = self
            .turns
            .lock()
            .expect("the turns lock should not be poisoned");
        let mut rows: Vec<(String, String, String)> = turns
            .iter()
            .filter(|(_, turn)| session.is_none_or(|id| turn.session_id == id))
            .map(|(id, turn)| (id.clone(), turn.session_id.clone(), turn.started_at.clone()))
            .collect();
        rows.sort_by(|a, b| a.2.cmp(&b.2));
        rows
    }

    /// Remove a turn. Once finished, there is nothing left to interrupt.
    pub(crate) fn finish_turn(&self, turn_id: &str) {
        let mut turns = self
            .turns
            .lock()
            .expect("the turns lock should not be poisoned");
        self.suspending
            .lock()
            .expect("the suspending lock should not be poisoned")
            .remove(turn_id);
        turns.remove(turn_id);
        drop(turns);
        // Same principle: unanswered approval requests for this turn are invalidated too (their receiver
        // is already gone; keeping them only takes up space).
        self.approvals
            .lock()
            .expect("the approvals lock should not be poisoned")
            .remove(turn_id);
    }

    /// A human pressed pause on the turn a task carries: mark it **and** interrupt it, in one
    /// step. The mark only lands when a live turn exists — otherwise a stale flag would make
    /// the *next* turn (a resume!) think it was paused, and finalize would keep it suspended.
    /// Returns false = no live turn (the task was idle; nothing to interrupt).
    pub(crate) fn suspend_turn(&self, turn_id: &str) -> bool {
        let turns = self
            .turns
            .lock()
            .expect("the turns lock should not be poisoned");
        let Some(turn) = turns.get(turn_id) else {
            return false;
        };
        self.suspending
            .lock()
            .expect("the suspending lock should not be poisoned")
            .insert(turn_id.to_string());
        turn.interrupt.cancel();
        true
    }

    /// Was this turn paused rather than stopped? Consumes the flag: one verdict per turn.
    pub(crate) fn take_suspending(&self, turn_id: &str) -> bool {
        self.suspending
            .lock()
            .expect("the suspending lock should not be poisoned")
            .remove(turn_id)
    }

    /// Interrupt a running turn. Returns false = not found (already finished).
    /// Exposed for eval: the interrupt scenario needs to flip the token mid-stream.
    pub fn interrupt_turn(&self, turn_id: &str) -> bool {
        let found = self
            .turns
            .lock()
            .expect("the turns lock should not be poisoned")
            .get(turn_id)
            .map(|turn| turn.interrupt.clone());
        match found {
            Some(interrupt) => {
                interrupt.cancel();
                true
            }
            None => false,
        }
    }

    /// The body of `config/write`: validate patch → accumulate and persist → apply → re-resolve provider.
    ///
    /// A validation failure returns an error with the reason and changes nothing — a patch is all-or-nothing;
    /// half a patch is worse than none.
    pub(crate) fn apply_config_patch(&self, patch: &SettingsPatch) -> Result<SettingsView, String> {
        validate_patch(patch)?;

        // Accumulate: saved patches are the foundation of new ones. If persisting fails, memory is left
        // untouched — "changed but not remembered" lies harder than "not changed".
        let home = self.settings().home.clone();
        let mut saved = joyczl_config::load_patch(&home);
        saved.merge_newer(patch);
        joyczl_config::save_patch(&home, &saved)
            .map_err(|e| format!("could not write settings.json: {e}"))?;

        let mut settings = self.settings();
        joyczl_config::apply_patch(&saved, &mut settings);

        // Changed provider / model means re-resolve. A resolution failure is not fatal — record the reason;
        // `turn/*` will spell out "missing key"; the old client has been replaced and must not be used again.
        let resolved = match joyczl_provider::resolve(&settings) {
            Ok(resolved) => Some(resolved),
            Err(reason) => {
                eprintln!("(joy) after config/write no model is configured; turn/* unavailable — {reason}");
                None
            }
        };
        self.set_settings(settings.clone());
        self.set_resolved(resolved);
        Ok(settings.view())
    }
}

/// Field validation for config/write: numeric bounds go through `joyczl-config`'s `BOUNDS`
/// (the same table as at startup).
///
/// This used to also check "the provider must exist" — now provider and models belong to
/// `<home>/models.json` (change them via `model/*`, validated in models.rs), so the patch
/// only carries runtime parameters and one bounds table is enough.
fn validate_patch(patch: &SettingsPatch) -> Result<(), String> {
    joyczl_config::validate_patch_values(patch)
}

/// Open state.db under `settings.home` and assemble.
pub async fn open(settings: &Settings) -> Result<Server> {
    // First stack the saved config/write patches on top: env vars are the foundation,
    // and explicit overrides in settings.json speak later and louder.
    let mut settings = settings.clone();
    let saved = joyczl_config::load_patch(&settings.home);
    joyczl_config::apply_patch(&saved, &mut settings);

    // ---- Startup-time validation: bad config exits on the spot.
    //
    // This is where the discipline "turn bad config into a startup error, not a runtime surprise" lands.
    // What is checked is the **final stacked value**: whether the env var or settings.json is wrong, it
    // gets reported in this one place, and it reports which variable/field is wrong.
    if let Err(why) = settings.validate() {
        anyhow::bail!(
            "The configuration has a problem, so Joy will not start: {why}\n( fix that variable, or drop the override in <home>/settings.json; the boundary table is in joyczl-config's BOUNDS and docs/configuration.md)"
        );
    }

    settings.ensure_home()?;
    let ownership = Arc::new(lock::acquire(&settings.home).map_err(anyhow::Error::msg)?);
    let pool = joyczl_state::open(&settings.home.join("state.db")).await?;
    Ok(Server::boot_owned(pool, settings, Some(ownership)).await)
}

/// The built-in tool table, plus the two live cells the approval answer writes into.
///
/// Bundled instead of growing the return tuple: every new shared cell would otherwise add a
/// positional element at every call site, and a swapped pair of `Arc`s compiles happily.
struct BuiltinTools {
    tools: ToolRegistry,
    jobs: Option<Arc<dyn joyczl_tools::jobs::JobRegistry>>,
    /// See `Server::exec_session_allow` — same `Arc` the exec policy holds.
    exec_session_allow: Arc<Mutex<Vec<String>>>,
    /// See `Server::write_approved_dirs` — same `Arc` the write policy holds.
    write_approved_dirs: Arc<Mutex<Vec<PathBuf>>>,
}

/// Built-in tools, plus MCP tools configured in `<home>/mcp.json`.
///
/// An unreachable MCP server only leaves a warning on stderr and Joy starts as usual —
/// a misconfigured server must not keep the whole assistant from starting (the same rule as
/// "a tool execution failure must not crash a turn"). Without mcp.json it is as if nothing was
/// configured: no file read, no process spawned, no network. Built-in tool table + background job
/// table (the job table is built alongside `run_command`; both share one exec policy, same return place).
/// The execution policy, built in one place.
///
/// Two callers: `builtin_tools` at boot (for `run_command` and the job host), and `turn::run_turn`
/// every turn (for declarative tools, which are re-read per turn — see `joyczl_tools::declarative`).
/// The `session_allow` cell is the same `Arc` in both, so an "always allow in this session"
/// answered for `run_command` also covers a declarative tool running the same command.
pub(crate) fn exec_policy(
    settings: &Settings,
    session_allow: Arc<Mutex<Vec<String>>>,
) -> joyczl_tools::exec::ExecPolicy {
    joyczl_tools::exec::ExecPolicy {
        allow: settings.exec_allow.clone(),
        timeout_secs: settings.exec_timeout_secs,
        network: settings.exec_network,
        // Approval only affects the allow-table gate: on no match, reject outright or ask once.
        approval: settings.approval == "on-request",
        approval_timeout_secs: settings.approval_timeout_secs,
        // Overlong output spills to disk: truncation still happens (the context must be protected), but the original remains.
        spill_dir: Some(settings.home.join("spill")),
        extra_roots: settings.exec_writable_roots.clone(),
        home: settings.home.clone(),
        session_allow,
    }
}

async fn builtin_tools(
    settings: &Settings,
    pool: sqlx::SqlitePool,
    notifier: Option<Arc<dyn joyczl_tools::notify::Notifier>>,
    execution_owner: Option<Arc<lock::DaemonLock>>,
) -> BuiltinTools {
    let mut tools = joyczl_tools::handlers::build_default();
    // The live half of "remember": approvals land here and take effect on the next call. The
    // persisted half (`settings.json`) is folded in below, at policy construction.
    let exec_session_allow: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let write_approved_dirs: Arc<Mutex<Vec<PathBuf>>> =
        Arc::new(Mutex::new(settings.write_allow_dirs.clone()));
    if !joyczl_tools::web::search_ready() {
        // Not silent: the model's tool table lost an entry — the human should know why.
        eprintln!(
            "(joy) search not ready: no search key configured and DDG is off (JOY_SEARCH_DDG=0); search_web was not registered"
        );
    }
    let mut jobs_handle: Option<Arc<dyn joyczl_tools::jobs::JobRegistry>> = None;
    // Exec tool: **off by default**, registered only when enabled — when off, the model cannot even
    // see its name. The highest-privilege thing there is; the switch must be pressed by the user's own hand.
    if settings.exec_enabled {
        let sandbox = if joyczl_tools::exec::sandbox_available() {
            "available"
        } else {
            "unavailable (commands will be refused)"
        };
        let allow = if settings.exec_allow.is_empty() {
            "empty (nothing is allowed)".to_string()
        } else {
            settings.exec_allow.join(", ")
        };
        let network = if settings.exec_network {
            "network allowed"
        } else {
            "network cut inside the sandbox (JOY_EXEC_NETWORK=1 enables it)"
        };
        let extra = if settings.exec_writable_roots.is_empty() {
            String::new()
        } else {
            format!(
                ", extra writable roots: {}",
                settings
                    .exec_writable_roots
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let approval = if settings.approval == "on-request" {
            format!(
                ", asks you when nothing matches ({}s without an answer counts as a denial)",
                settings.approval_timeout_secs
            )
        } else {
            ", refuses outright when nothing matches (JOY_APPROVAL=on-request asks instead)"
                .to_string()
        };
        eprintln!(
            "(joy) exec tool enabled: sandbox {sandbox}, {network}, allow rules: {allow}{approval}{extra}"
        );
        // Say up front what the sandbox will *not* write: the home is the control plane
        // (settings.json decides this very allow table), so only the workspace and the
        // explicitly listed extra roots are writable.
        let dropped =
            joyczl_tools::roots::resolve_roots(&[], &settings.exec_writable_roots, &settings.home)
                .dropped;
        if !dropped.is_empty() {
            eprintln!(
                "(joy) these extra writable roots were refused (they would hand over the control plane: home itself, an ancestor of home, or an entry in HOME_CONTROL): {}",
                dropped
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        // Shared with the server: an approval answered "always allow in this session" is visible to the very
        // next call, not to the next process.
        let policy = exec_policy(settings, exec_session_allow.clone());
        tools.register_exclusive(joyczl_tools::exec::run_command(policy.clone()));
        // Declarative tools: the model writes a JSON definition, the next turn can call it. It
        // lives here because the switch is the same one — a definition is inert without exec,
        // and `define_tool` would be a tool that cannot do anything.
        tools.register_exclusive(joyczl_tools::declarative::define_tool(
            settings.home.clone(),
        ));

        // Background jobs: share the same policy with run_command (sandbox, no network, allow table,
        // approval all identical) — background is not a side door around them.
        let ledger = joyczl_state::Jobs::new(pool.clone());
        let jobs_impl = jobs::Jobs::new(policy, settings.home.clone(), ledger)
            .with_notifier(notifier.clone())
            .with_execution_owner(execution_owner);
        jobs_impl.reconcile().await;
        let jobs: Arc<dyn joyczl_tools::jobs::JobRegistry> = Arc::new(jobs_impl);
        tools.register(joyczl_tools::jobs::job_output(Some(jobs.clone())));
        tools.register(joyczl_tools::jobs::job_list(Some(jobs.clone())));
        tools.register(joyczl_tools::jobs::job_kill(Some(jobs.clone())));
        jobs_handle = Some(jobs);
        eprintln!(
            "(joy) background jobs available: run_command with background=true, plus job_output / job_list / job_kill"
        );
    }
    // File-write tools: **off by default**, registered only when JOY_WRITE=1 — same discipline
    // as run_command: the switch must be pressed by the user's own hand. Writable boundary is
    // the same definition the sandbox uses (`joyczl_tools::roots`): workdir +
    // JOY_EXEC_WRITABLE_ROOTS, never home; approval in on-request mode asks once per write.
    if settings.write_enabled {
        let policy = joyczl_tools::fs_write::WritePolicy {
            approval: settings.approval == "on-request",
            approval_timeout_secs: settings.approval_timeout_secs,
            extra_roots: settings.exec_writable_roots.clone(),
            // Persisted "remember this one" choices plus whatever this session answers from now on.
            approved_dirs: write_approved_dirs.clone(),
        };
        tools.register_exclusive(joyczl_tools::fs_write::write_file(policy.clone()));
        tools.register_exclusive(joyczl_tools::fs_write::edit(policy.clone()));
        tools.register_exclusive(joyczl_tools::patch::apply_patch(policy.clone()));
        tools.register_exclusive(joyczl_tools::fs_manage::delete_file(policy.clone()));
        tools.register_exclusive(joyczl_tools::fs_manage::move_file(policy));
        eprintln!(
            "(joy) file tools enabled: write_file / edit / apply_patch / delete_file / move_file (writable = this turn's working directory + extra roots; with no directory picked nothing can be written; with JOY_APPROVAL=on-request every write asks)"
        );
    }
    // Connectors: `<home>/connectors.json` **is** the switch — no file, no connector tools
    // (the model cannot even see their names). A configured-but-unusable connector is not
    // registered either: a tool that is guaranteed to fail only invites doomed calls.
    match joyczl_connector::config::load(&settings.home) {
        Ok(config) => {
            if let Some(feishu) = config.feishu.clone() {
                // Sending as a person uses a rotating token chain: renew it in the background
                // (ten minutes before each expiry) so a send never pays the refresh latency,
                // and a laptop that slept through the expiry wakes up with a valid token.
                if feishu.identity() == Ok("user") {
                    joyczl_connector::feishu_oauth::spawn_renewer(
                        feishu.clone(),
                        settings.home.clone(),
                    );
                    eprintln!("(joy) Feishu sends as the user: the user token renews itself in the background");
                }
                if feishu.allow_recipients.is_empty() {
                    eprintln!(
                        "(joy) connector feishu is configured but its allowlist is empty (nobody can receive) — feishu_send was not registered; fill in allow_recipients in {}",
                        settings.home.join("connectors.json").display()
                    );
                } else {
                    let mode = if feishu.live {
                        "live (every send asks a person first)"
                    } else {
                        "draft mode (writes to outbox only, never sends)"
                    };
                    tools.register(joyczl_connector::feishu_send_tool(feishu));
                    eprintln!("(joy) connector feishu enabled: feishu_send — {mode}");
                }
            }
            if let Some(email) = config.email {
                if email.allow_recipients.is_empty() {
                    eprintln!(
                        "(joy) connector email is configured but its allowlist is empty (nobody can send) — email_send was not registered; fill in allow_recipients in {}",
                        settings.home.join("connectors.json").display()
                    );
                } else {
                    let mode = if email.live {
                        "live (every send asks a person first)"
                    } else {
                        "draft mode (writes to outbox only, never sends)"
                    };
                    tools.register(joyczl_connector::email_send_tool(email));
                    eprintln!("(joy) connector email enabled: email_send — {mode}");
                }
            }
        }
        Err(e) => eprintln!(
            "(joy) the connector configuration has a problem; no connectors registered: {e}"
        ),
    }
    // Sweep spill/ while at it at startup: keep only the last 7 days (see the "no quota rotation" item
    // in docs/limitations.md). Unrelated to the exec switch — if the files are there, they get swept.
    joyczl_tools::exec::prune_spill(&settings.home, 7);

    // The user's mcp.json plus JOY_MCP_CONFIG's extras (operator-level, see
    // joyczl_mcp::config::config_paths_for) — the dashboard greys the extras out.
    let mcp = joyczl_mcp::McpClient::connect_home(&settings.home).await;
    for warning in mcp.warnings() {
        // Same as above: stdout is the protocol channel; logs may only go to stderr.
        eprintln!("(joy) {warning}");
    }
    let servers = mcp.servers();
    let mcp_tools = mcp.tools();
    let count = mcp_tools.len();
    for tool in mcp_tools {
        // Remote schemas do not prove read-only behavior. Serialize unknown effects.
        tools.register_exclusive(tool);
    }
    if !servers.is_empty() {
        eprintln!(
            "(joy) MCP: connected {} servers ({count} tools): {}",
            servers.len(),
            servers.join(", ")
        );
    }
    BuiltinTools {
        tools,
        jobs: jobs_handle,
        exec_session_allow,
        write_approved_dirs,
    }
}

/// The **only** construction site of the tool execution environment. Both turn and subagent go through it —
/// writing it twice is just waiting for the day a field is added and one site is missed (a subagent with
/// half a ctx is very hard to debug).
/// The in-turn tool-result budget (`JOY_TOOL_RESULT_*`). Turn and subagent share one policy — the subagent inherits the parent's discipline.
pub(crate) fn tool_result_budget(settings: &Settings) -> joyczl_loop::budget::ToolResultBudget {
    joyczl_loop::budget::ToolResultBudget {
        total_chars: settings.tool_result_total_chars.max(0) as usize,
        per_result_chars: settings.tool_result_max_chars.max(0) as usize,
        keep_recent: settings.tool_result_keep_recent.max(0) as usize,
    }
}

/// What gets injected into the handler for this turn: session name, approval channel, hooks.
///
/// Bundled into a struct instead of adding more parameters: they **always change together** (each new
/// injected handle would add a line at four call sites), and clippy stops at the 8th parameter anyway.
#[derive(Clone, Default)]
pub(crate) struct Injections {
    pub session_id: String,
    /// This turn's working directory; None = the process cwd.
    pub cwd: Option<std::path::PathBuf>,
    pub approval: Option<Arc<dyn joyczl_tools::approval::ApprovalBroker>>,
    pub hooks: Option<Arc<joyczl_tools::hooks::Hooks>>,
    pub jobs: Option<Arc<dyn joyczl_tools::jobs::JobRegistry>>,
}

pub(crate) fn tool_ctx(
    facts: &Facts,
    episodes: &Episodes,
    chat: &Chat,
    calendar: &Calendar,
    home: &std::path::Path,
    injected: Injections,
) -> ToolCtx {
    ToolCtx {
        on_uncertain: None,
        resource_lease: None,
        task_run_id: None,
        activate: None,
        embedder: None,
        facts: facts.clone(),
        episodes: episodes.clone(),
        chat: chat.clone(),
        calendar: calendar.clone(),
        home: home.to_path_buf(),
        session_id: injected.session_id,
        cwd: injected.cwd,
        approval: injected.approval,
        hooks: injected.hooks,
        turn_limits: std::sync::Arc::new(joyczl_tools::web::TurnLimits::from_env()),
        jobs: injected.jobs,
        // Set per call by the loop when a client is watching (SSE); a turn with no observer
        // (CLI, eval, jobs) keeps it `None` and the tool just collects its output.
        on_delta: None,
    }
}

/// Fire one lifecycle event (a zero-cost no-op when hooks are unconfigured).
///
/// Lifecycle events are fired on the app-server side: only it knows when `SessionStart`/`Stop` and the
/// like happen; tool events are fired inside `ToolRegistry::execute` (that is the only choke point).
pub(crate) async fn fire_hook(
    hooks: &Option<Arc<joyczl_tools::hooks::Hooks>>,
    event: joyczl_tools::hooks::HookEvent,
    payload: serde_json::Value,
) -> joyczl_tools::hooks::HookOutcome {
    match hooks {
        Some(hooks) => hooks.fire(event, payload).await,
        None => joyczl_tools::hooks::HookOutcome::default(),
    }
}

/// Integer fields in the protocol are i32 (see joyczl-protocol's module docs for the reason); the storage layer is i64.
/// Out-of-range values are clamped to the bounds instead of panicking — if the memory store really held
/// 2.1 billion facts, garbled numbering is still far better than the whole app-server crashing.
pub(crate) fn narrow(value: i64) -> i32 {
    value.clamp(0, i32::MAX as i64) as i32
}

/// Add `value` to a shared list unless it is already there (an approval answered twice in one turn
/// must not grow the table).
///
/// A poisoned lock is recovered rather than propagated: remembering is a convenience, and a thread
/// that panicked elsewhere must not be able to take the turn down here.
fn push_unique<T: PartialEq>(cell: &Mutex<Vec<T>>, value: T) {
    let mut list = match cell.lock() {
        Ok(list) => list,
        Err(poisoned) => poisoned.into_inner(),
    };
    if !list.contains(&value) {
        list.push(value);
    }
}

#[cfg(test)]
mod task_dispatch_tests;

#[cfg(test)]
mod task_recovery_tests;

#[cfg(test)]
mod completion_tests;

#[cfg(test)]
mod input_wake_tests;

#[cfg(test)]
mod task_operations_tests;

#[cfg(test)]
mod input_control_tests;

#[cfg(test)]
mod runtime_ownership_tests;

#[cfg(test)]
mod turn_operations_tests;

#[cfg(test)]
mod recovery_usage_tests;

mod quick_context;

#[cfg(test)]
mod memory_recovery_tests;
