//! Runtime responsibilities of the app-server.

use crate::{
    builtin_tools, lock, memory_worker, models, resources, subagent, BuiltinTools, EventSink,
    Server,
};
use anyhow::Result;
use joyczl_config::Settings;
use joyczl_state::{Calendar, Chat, Episodes, Facts};
use sqlx::sqlite::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::Mutex as AsyncMutex;

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
    pub(crate) async fn restore_tasks(&self) {
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
        crate::task::dispatch::kick(self.clone(), dispatch_sink);
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
                    crate::task::waits::arm(self.clone(), sink.clone(), wait);
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
