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

mod approval;
mod dispatch;
mod goal;
mod inbox;
mod jobs;
/// Guardian lock: only one app-server per home (`joyczl_app_server::lock`).
pub mod lock;
pub mod models;
mod resume;
mod route;
mod stdio;
mod subagent;
mod task;
mod trace;
mod turn;

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
#[path = "subagent_tests.rs"]
mod subagent_tests;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use joyczl_config::Settings;
use joyczl_loop::Interrupt;
use joyczl_protocol::{JsonRpcMessage, ServerNotification, SettingsPatch, SettingsView};
use joyczl_provider::Resolved;
use joyczl_state::{Calendar, Chat, Episodes, Facts};
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
/// and flips the switch. A finished turn (success or failure) is removed.
///
/// `Clone`: every field is a handle (Arc / connection pool / config); copying one does not copy state.
/// The full_agent node in the graph is a 'static closure and can only hold what it owns —
/// what it holds is exactly this clone.
#[derive(Clone)]
pub struct Server {
    pub(crate) facts: Facts,
    pub(crate) episodes: Episodes,
    pub(crate) chat: Chat,
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
    pub(crate) turns: Arc<Mutex<HashMap<String, Arc<Interrupt>>>>,
    /// Approval requests waiting for a human: `approval/respond` finds one by turn_id + request_id
    /// and delivers the answer back. Same lifecycle discipline as `turns` (the whole bucket is dropped when a turn ends).
    pub(crate) approvals: approval::Pending,
    /// Lifecycle hooks (`<home>/hooks.json`). `None` = unconfigured or disabled.
    pub(crate) hooks: Option<Arc<joyczl_tools::hooks::Hooks>>,
    /// Session -> todo list (see todo.rs in joyczl-tools). Injected into the system prompt every turn,
    /// so it does not depend on history and compaction cannot wash it away.
    pub(crate) todo: Arc<joyczl_tools::todo::TodoBoard>,
    /// Background job table (switched on/off together with `JOY_EXEC`).
    pub(crate) jobs: Option<Arc<dyn joyczl_tools::jobs::JobRegistry>>,
    /// Session -> goal (see goal.rs). Only a human can set one; the model cannot even see this path.
    /// This is the **runner's mirror**: the truth lives in `tasks` (agent_tasks, migration 0014);
    /// boot restores the mirror from it, so a restart no longer loses the target.
    pub(crate) goals: goal::Goals,
    /// Connector inbox (migration 0015): what came in from the outside, recorded as-is.
    /// Only the poller and `inbox/*` touch it; the model has no path here at all.
    pub(crate) inbox: joyczl_state::Inbox,
    /// Long tasks (migration 0014): identity, status, round account. Human-only switches live
    /// in task.rs; the goal loop above is the engine that drives their rounds.
    pub(crate) tasks: joyczl_state::Tasks,
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
    #[allow(dead_code)]
    pub(crate) pool: SqlitePool,
}

impl Server {
    /// Assembled from state.db. A provider-resolution failure is not fatal — record the reason and report it at turn time.
    /// Set or clear a goal. **Only a human can reach here** — the model has no `goal/set` tool.
    ///
    /// dispatch and the REPL share one implementation: two copies would eventually forget a
    /// validation or the `max_rounds` on one of them.
    pub async fn goal_set(
        &self,
        session_id: &str,
        condition: Option<&str>,
    ) -> joyczl_protocol::GoalSetResponse {
        let trimmed = condition.map(str::trim).filter(|text| !text.is_empty());
        let active = self.tasks.active(session_id).await.ok().flatten();
        match (trimmed, active) {
            // Setting a goal on a session with no task: the goal **creates** the task — the
            // compatibility layer that keeps `/goal` and the task card pointing at one row.
            (Some(condition), None) => {
                let max_rounds = self
                    .settings
                    .read()
                    .expect("settings 锁不该中毒")
                    .goal_max_rounds;
                let id = format!("task{}", chrono::Local::now().timestamp_millis());
                if let Err(e) = self
                    .tasks
                    .create(
                        &id,
                        session_id,
                        condition,
                        Some(condition),
                        max_rounds,
                        None,
                    )
                    .await
                {
                    eprintln!("(joy) 目标没建成任务（目标仍在内存里跑）：{e}");
                }
            }
            (Some(condition), Some(task)) => {
                if let Err(e) = self.tasks.set_goal(&task.id, Some(condition)).await {
                    eprintln!("(joy) 目标没写进任务台账（内存里仍然生效）：{e}");
                }
            }
            // Clearing: the task survives as a single-turn one (its title remains the record).
            (None, Some(task)) => {
                if let Err(e) = self.tasks.set_goal(&task.id, None).await {
                    eprintln!("(joy) 清目标没写进任务台账：{e}");
                }
            }
            (None, None) => {}
        }
        goal::set(&self.goals, session_id, trimmed);
        self.goal_view(session_id).await
    }

    /// The home this server was booted with (connectors.json, outbox — same file the
    /// ops side reads; one machine, one truth).
    pub(crate) fn home(&self) -> std::path::PathBuf {
        self.settings
            .read()
            .expect("settings 锁不该中毒")
            .home
            .clone()
    }

    pub async fn boot(pool: SqlitePool, mut settings: Settings) -> Self {
        // The model profile picked in the UI is newer and speaks louder than env vars typed at startup.
        let secrets = models::new_secrets();
        models::apply_at_boot(&mut settings, &secrets);
        let models = models::new_models(joyczl_config::models::load_models(&settings.home));
        // When routing is on, make it visible: which file, which mode — step one of explaining "why this turn used that model".
        let routes_boot = joyczl_config::routes::load_routes(&settings.home);
        if routes_boot.mode == joyczl_config::routes::RouteMode::Auto {
            eprintln!(
                "(joy) 模型路由：auto（{}，{} 个档位映射）",
                joyczl_config::routes::routes_path(&settings.home).display(),
                routes_boot.routes.len()
            );
        }
        let route_cache = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let settings = Arc::new(RwLock::new(settings));
        // The read lock is taken only inside an expression: **never held across an await** (clippy would
        // stop it, and rightly so — a read lock stuck on an await would clog the whole server).
        let snapshot = settings.read().expect("settings 锁不该中毒").clone();
        // The merged model catalog: built-in table + the user's override file. Loaded once —
        // switching models re-resolves, it does not re-read the catalog.
        let catalog = joyczl_provider::catalog::Catalog::load(&snapshot.home);
        let resolved = match joyczl_provider::resolve_with(&snapshot, &catalog) {
            Ok(resolved) => Some(resolved),
            Err(reason) => {
                // Log to stderr: stdout is the protocol channel and must not mix in logs.
                eprintln!("(joy) 模型还没配好，turn/* 暂不可用 —— {reason}");
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
                eprintln!("(joy) 技能 '{skill}' 依赖的 '{missing}' 不在，它不参与隐式触发");
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
                    eprintln!("(joy) hooks 已启用：{}", hooks.summary());
                    Some(Arc::new(hooks))
                }
                None => {
                    eprintln!(
                        "(joy) JOY_HOOKS=1 但 {} 不存在或一条都没配 —— 这次不启用",
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
        let (mut tools, jobs_handle) =
            builtin_tools(&snapshot, pool.clone(), notifier.clone()).await;
        tools.set_hooks(hooks.clone());
        let facts = Facts::new(pool.clone());
        let episodes = Episodes::new(pool.clone());
        let chat = Chat::new(pool.clone());
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
                "(joy) 子代理已启用：delegate_task 可用了（子代理不能再派生子代理、不参与批准）"
            );
            tools.register(joyczl_tools::subagent::delegate_task(Arc::new(
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
            hooks,
            todo,
            jobs: jobs_handle,
            goals: goal::store(),
            tasks: joyczl_state::Tasks::new(pool.clone()),
            suspending: Arc::new(Mutex::new(HashSet::new())),
            models,
            route_cache,
            catalog,
            session_locks: Arc::new(Mutex::new(HashMap::new())),
            secrets,
            pool,
        };
        server.restore_tasks().await;
        server
    }

    /// Boot restore for long tasks (migration 0014):
    /// 1. a `running` row means the previous process died while carrying it — mark it suspended
    ///    with the honest reason (the turn's `open` checkpoint may then flip it back to running);
    /// 2. load surviving goals into the runner's mirror — a restart no longer forgets the target.
    async fn restore_tasks(&self) {
        match self.tasks.reconcile_restart().await {
            Ok(rows) if !rows.is_empty() => {
                for row in &rows {
                    eprintln!(
                        "(joy) 对账：任务「{}」在进程退出时还在跑，先标挂起（会话 {}）",
                        row.title, row.session_id
                    );
                }
            }
            Ok(_) => {}
            Err(e) => eprintln!("(joy) 任务对账读不了库（不挡启动）：{e}"),
        }
        match self.tasks.list_active().await {
            Ok(rows) => {
                for row in rows {
                    if let Some(condition) = &row.goal_condition {
                        goal::set(&self.goals, &row.session_id, Some(condition));
                    }
                }
            }
            Err(e) => eprintln!("(joy) 活跃任务读不出来（目标不会自动恢复）：{e}"),
        }
    }

    /// The session's write-section lock — created on first use, held forever.
    /// Two hash lookups are nothing next to what the lock actually protects.
    pub(crate) fn session_write_lock(&self, session_id: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self.session_locks.lock().expect("session_locks 中毒");
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

    /// This turn's approval channel. `never` mode (the default) returns `None` — nobody asks, so nobody
    /// answers, and actions requiring approval are rejected outright.
    /// Dashboard read surface: the session's goal state. No goal = `active: false` —
    /// the shape is the same struct `goal/set` answers with, one source of truth.
    ///
    /// Since 0.11.0 the answer is read from the **task row**: the mirror in memory drives the
    /// loop, but the ledger is what the human sees across restarts.
    pub async fn goal_view(&self, session_id: &str) -> joyczl_protocol::GoalSetResponse {
        let settings_max = self
            .settings
            .read()
            .expect("settings 锁不该中毒")
            .goal_max_rounds;
        let task = self.tasks.active(session_id).await.ok().flatten();
        match task {
            Some(task) => joyczl_protocol::GoalSetResponse {
                active: task.goal_condition.is_some(),
                condition: task.goal_condition,
                rounds_used: task.rounds_used,
                max_rounds: task.max_rounds.max(settings_max),
            },
            None => joyczl_protocol::GoalSetResponse {
                active: false,
                condition: None,
                rounds_used: 0,
                max_rounds: settings_max,
            },
        }
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

    /// Push goal endings outward (M3a): satisfied/impossible/round-limit/blocked all count —
    /// when the autonomous loop stops, a human should know, however ungracefully it stopped.
    pub(crate) fn notify_goal_finished(&self, session_id: &str, status: &str, reason: &str) {
        if let Some(notifier) = &self.notifier {
            notifier.notify(joyczl_tools::notify::NotifyEvent::GoalFinished {
                session_id: session_id.to_string(),
                status: status.to_string(),
                reason: reason.to_string(),
            });
        }
    }

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
    pub fn answer_approval(
        &self,
        turn_id: &str,
        request_id: &str,
        approved: bool,
        remember: bool,
    ) -> bool {
        let waiting = self
            .approvals
            .lock()
            .expect("approvals 锁不该中毒")
            .get_mut(turn_id)
            .and_then(|per_turn| per_turn.remove(request_id));
        let Some(waiting) = waiting else {
            return false;
        };
        if approved && remember {
            self.remember_command(&waiting.command);
        }
        // Failing to deliver (the receiver is already gone) is not a failure: that turn has ended.
        let _ = waiting.tx.send(approved);
        true
    }

    /// "Remember this command": replace the allow table wholesale and persist into `settings.json`.
    ///
    /// What is remembered is **this command itself**, no wildcards — what the user saw and approved was
    /// it, not the whole class. The exec policy is read once at startup, so this takes effect **on the
    /// next start**; say that honestly, instead of letting people believe it will not ask again.
    fn remember_command(&self, command: &str) {
        let mut settings = self.settings();
        if settings.exec_allow.iter().any(|rule| rule == command) {
            return; // Already in the table (probably asked twice in the same turn)
        }
        settings.exec_allow.push(command.to_string());
        let patch = joyczl_protocol::SettingsPatch {
            exec_allow: Some(settings.exec_allow.clone()),
            ..Default::default()
        };
        match self.apply_config_patch(&patch) {
            Ok(_) => eprintln!(
                "(joy) 已记住这条命令：{command}（执行策略在启动时读一次，下次启动才生效）"
            ),
            Err(why) => eprintln!("(joy) 没能记住这条命令：{why}"),
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
        self.settings.read().expect("settings 锁不该中毒").clone()
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

    /// A snapshot of the current provider resolution (None when no key is configured).
    pub fn resolved(&self) -> Option<Resolved> {
        self.resolved.read().expect("resolved 锁不该中毒").clone()
    }

    pub(crate) fn set_settings(&self, settings: Settings) {
        *self.settings.write().expect("settings 锁不该中毒") = settings;
    }

    pub(crate) fn set_resolved(&self, resolved: Option<Resolved>) {
        *self.resolved.write().expect("resolved 锁不该中毒") = resolved;
    }

    /// Inject a provider resolution. The injection point for eval and tests: the normal path goes
    /// through `joyczl_provider::resolve` (env vars supply the key); eval needs a scripted model to
    /// pin harness behavior offline and deterministically — that is when one is plugged in from outside.
    pub fn install_provider(&self, resolved: Resolved) {
        self.set_resolved(Some(resolved));
    }

    /// Register a running turn and hand out its cancellation token. `finish_turn` must be called when the turn ends.
    pub(crate) fn register_turn(&self, turn_id: &str) -> Arc<Interrupt> {
        let interrupt = Interrupt::new();
        self.turns
            .lock()
            .expect("turns 锁不该中毒")
            .insert(turn_id.to_string(), interrupt.clone());
        interrupt
    }

    /// Remove a turn. Once finished, there is nothing left to interrupt.
    pub(crate) fn finish_turn(&self, turn_id: &str) {
        self.turns.lock().expect("turns 锁不该中毒").remove(turn_id);
        // Same principle: unanswered approval requests for this turn are invalidated too (their receiver
        // is already gone; keeping them only takes up space).
        self.approvals
            .lock()
            .expect("approvals 锁不该中毒")
            .remove(turn_id);
    }

    /// A human pressed pause on the turn a task carries: mark it **and** interrupt it, in one
    /// step. The mark only lands when a live turn exists — otherwise a stale flag would make
    /// the *next* turn (a resume!) think it was paused, and finalize would keep it suspended.
    /// Returns false = no live turn (the task was idle; nothing to interrupt).
    pub(crate) fn suspend_turn(&self, turn_id: &str) -> bool {
        let interrupt = self
            .turns
            .lock()
            .expect("turns 锁不该中毒")
            .get(turn_id)
            .cloned();
        let Some(interrupt) = interrupt else {
            return false;
        };
        self.suspending
            .lock()
            .expect("suspending 锁不该中毒")
            .insert(turn_id.to_string());
        interrupt.cancel();
        true
    }

    /// Was this turn paused rather than stopped? Consumes the flag: one verdict per turn.
    pub(crate) fn take_suspending(&self, turn_id: &str) -> bool {
        self.suspending
            .lock()
            .expect("suspending 锁不该中毒")
            .remove(turn_id)
    }

    /// Interrupt a running turn. Returns false = not found (already finished).
    /// Exposed for eval: the interrupt scenario needs to flip the token mid-stream.
    pub fn interrupt_turn(&self, turn_id: &str) -> bool {
        let found = self
            .turns
            .lock()
            .expect("turns 锁不该中毒")
            .get(turn_id)
            .cloned();
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
            .map_err(|e| format!("写 settings.json 失败：{e}"))?;

        let mut settings = self.settings();
        joyczl_config::apply_patch(&saved, &mut settings);

        // Changed provider / model means re-resolve. A resolution failure is not fatal — record the reason;
        // `turn/*` will spell out "missing key"; the old client has been replaced and must not be used again.
        let resolved = match joyczl_provider::resolve(&settings) {
            Ok(resolved) => Some(resolved),
            Err(reason) => {
                eprintln!("(joy) config/write 后模型还没配好，turn/* 暂不可用 —— {reason}");
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
            "配置有问题，Joy 不启动：{why}\n\
             （改好那个变量，或删掉 <home>/settings.json 里对应的覆盖；\
             边界表见 joyczl-config 的 BOUNDS 与 docs/configuration.md）"
        );
    }

    settings.ensure_home()?;
    let pool = joyczl_state::open(&settings.home.join("state.db")).await?;
    Ok(Server::boot(pool, settings).await)
}

/// Built-in tools, plus MCP tools configured in `<home>/mcp.json`.
///
/// An unreachable MCP server only leaves a warning on stderr and Joy starts as usual —
/// a misconfigured server must not keep the whole assistant from starting (the same rule as
/// "a tool execution failure must not crash a turn"). Without mcp.json it is as if nothing was
/// configured: no file read, no process spawned, no network. Built-in tool table + background job
/// table (the job table is built alongside `run_command`; both share one exec policy, same return place).
async fn builtin_tools(
    settings: &Settings,
    pool: sqlx::SqlitePool,
    notifier: Option<Arc<dyn joyczl_tools::notify::Notifier>>,
) -> (
    ToolRegistry,
    Option<Arc<dyn joyczl_tools::jobs::JobRegistry>>,
) {
    let mut tools = joyczl_tools::handlers::build_default();
    if !joyczl_tools::web::search_ready() {
        // Not silent: the model's tool table lost an entry — the human should know why.
        eprintln!(
            "(joy) 搜索未就绪：没有配置搜索 key 且 DDG 已关（JOY_SEARCH_DDG=0），\
             search_web 未注册"
        );
    }
    let mut jobs_handle: Option<Arc<dyn joyczl_tools::jobs::JobRegistry>> = None;
    // Exec tool: **off by default**, registered only when enabled — when off, the model cannot even
    // see its name. The highest-privilege thing there is; the switch must be pressed by the user's own hand.
    if settings.exec_enabled {
        let sandbox = if joyczl_tools::exec::sandbox_available() {
            "可用"
        } else {
            "不可用（命令会被拒绝执行）"
        };
        let allow = if settings.exec_allow.is_empty() {
            "空（什么都不放行）".to_string()
        } else {
            settings.exec_allow.join(", ")
        };
        let network = if settings.exec_network {
            "可联网"
        } else {
            "沙箱内断网（JOY_EXEC_NETWORK=1 可开）"
        };
        let extra = if settings.exec_writable_roots.is_empty() {
            String::new()
        } else {
            format!(
                "，额外可写根：{}",
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
                "，没匹配上时问你（{}秒没人答就拒绝）",
                settings.approval_timeout_secs
            )
        } else {
            "，没匹配上直接拒绝（JOY_APPROVAL=on-request 可以改成问一句）".to_string()
        };
        eprintln!(
            "(joy) 执行工具已启用：沙箱 {sandbox}，{network}，放行规则：{allow}{approval}{extra}"
        );
        let policy = joyczl_tools::exec::ExecPolicy {
            allow: settings.exec_allow.clone(),
            timeout_secs: settings.exec_timeout_secs,
            network: settings.exec_network,
            // Approval only affects the allow-table gate: on no match, reject outright or ask once.
            approval: settings.approval == "on-request",
            approval_timeout_secs: settings.approval_timeout_secs,
            // Overlong output spills to disk: truncation still happens (the context must be protected), but the original remains.
            spill_dir: Some(settings.home.join("spill")),
            extra_roots: settings.exec_writable_roots.clone(),
        };
        tools.register(joyczl_tools::exec::run_command(policy.clone()));

        // Background jobs: share the same policy with run_command (sandbox, no network, allow table,
        // approval all identical) — background is not a side door around them.
        let ledger = joyczl_state::Jobs::new(pool.clone());
        let jobs_impl =
            jobs::Jobs::new(policy, settings.home.clone(), ledger).with_notifier(notifier.clone());
        jobs_impl.reconcile().await;
        let jobs: Arc<dyn joyczl_tools::jobs::JobRegistry> = Arc::new(jobs_impl);
        tools.register(joyczl_tools::jobs::job_output(Some(jobs.clone())));
        tools.register(joyczl_tools::jobs::job_list(Some(jobs.clone())));
        tools.register(joyczl_tools::jobs::job_kill(Some(jobs.clone())));
        jobs_handle = Some(jobs);
        eprintln!(
            "(joy) 后台作业可用：run_command 的 background=true，配 job_output / job_list / job_kill"
        );
    }
    // File-write tools: **off by default**, registered only when JOY_WRITE=1 — same discipline
    // as run_command: the switch must be pressed by the user's own hand. Writable boundary is
    // the sandbox's roots (workdir + home + JOY_EXEC_WRITABLE_ROOTS); approval in on-request
    // mode asks once per write.
    if settings.write_enabled {
        let policy = joyczl_tools::fs_write::WritePolicy {
            approval: settings.approval == "on-request",
            approval_timeout_secs: settings.approval_timeout_secs,
            extra_roots: settings.exec_writable_roots.clone(),
        };
        tools.register(joyczl_tools::fs_write::write_file(policy.clone()));
        tools.register(joyczl_tools::fs_write::edit(policy));
        eprintln!(
            "(joy) 文件写入工具已启用：write_file / edit（可写范围 = 工作目录 + Joy home + \
             额外可写根；JOY_APPROVAL=on-request 时每次写入都会问）"
        );
    }
    // Connectors: `<home>/connectors.json` **is** the switch — no file, no connector tools
    // (the model cannot even see their names). A configured-but-unusable connector is not
    // registered either: a tool that is guaranteed to fail only invites doomed calls.
    match joyczl_connector::config::load(&settings.home) {
        Ok(config) => {
            if let Some(feishu) = config.feishu.clone() {
                if feishu.allow_recipients.is_empty() {
                    eprintln!(
                        "(joy) 连接器 feishu 配置了但白名单是空的（谁都不能收）—— \
                         feishu_send 未注册；在 {} 里填 allow_recipients",
                        settings.home.join("connectors.json").display()
                    );
                } else {
                    let mode = if feishu.live {
                        "live（每次实际发送都会问人）"
                    } else {
                        "草稿模式（只写 outbox，不发）"
                    };
                    tools.register(joyczl_connector::feishu_send_tool(feishu));
                    eprintln!("(joy) 连接器 feishu 已启用：feishu_send —— {mode}");
                }
            }
            if let Some(email) = config.email {
                if email.allow_recipients.is_empty() {
                    eprintln!(
                        "(joy) 连接器 email 配置了但白名单是空的（谁都不能发）—— \
                         email_send 未注册；在 {} 里填 allow_recipients",
                        settings.home.join("connectors.json").display()
                    );
                } else {
                    let mode = if email.live {
                        "live（每次实际发送都会问人）"
                    } else {
                        "草稿模式（只写 outbox，不发）"
                    };
                    tools.register(joyczl_connector::email_send_tool(email));
                    eprintln!("(joy) 连接器 email 已启用：email_send —— {mode}");
                }
            }
        }
        Err(e) => eprintln!("(joy) 连接器配置有问题，未注册任何连接器：{e}"),
    }
    // Sweep spill/ while at it at startup: keep only the last 7 days (see the "no quota rotation" item
    // in docs/limitations.md). Unrelated to the exec switch — if the files are there, they get swept.
    joyczl_tools::exec::prune_spill(&settings.home, 7);

    let mcp = joyczl_mcp::McpClient::connect(&settings.home.join("mcp.json")).await;
    for warning in mcp.warnings() {
        // Same as above: stdout is the protocol channel; logs may only go to stderr.
        eprintln!("(joy) {warning}");
    }
    let servers = mcp.servers();
    let mcp_tools = mcp.tools();
    let count = mcp_tools.len();
    for tool in mcp_tools {
        tools.register(tool);
    }
    if !servers.is_empty() {
        eprintln!(
            "(joy) MCP：接上 {} 个服务器（{count} 个工具）：{}",
            servers.len(),
            servers.join(", ")
        );
    }
    (tools, jobs_handle)
}

/// The **only** construction site of the tool execution environment. Both turn and subagent go through it —
/// writing it twice is just waiting for the day a field is added and one site is missed (a subagent with
/// half a ctx is very hard to debug).
/// The in-turn tool-result budget (`JOY_TOOL_RESULT_*`). Turn and subagent share one policy — the subagent inherits the parent's discipline.
pub(crate) fn tool_result_budget(settings: &Settings) -> joyczl_loop::budget::ToolResultBudget {
    joyczl_loop::budget::ToolResultBudget {
        total_chars: settings.tool_result_total_chars.max(0) as usize,
        per_result_chars: settings.tool_result_max_chars.max(0) as usize,
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
