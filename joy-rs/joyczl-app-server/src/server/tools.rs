//! Tools responsibilities of the app-server.

use joyczl_config::Settings;
use joyczl_state::{Calendar, Chat, Episodes, Facts};
use joyczl_tools::ToolCtx;
use joyczl_tools::ToolRegistry;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::{jobs, lock, Server};

impl Server {
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
}

/// The built-in tool table, plus the two live cells the approval answer writes into.
///
/// Bundled instead of growing the return tuple: every new shared cell would otherwise add a
/// positional element at every call site, and a swapped pair of `Arc`s compiles happily.
pub(crate) struct BuiltinTools {
    pub(crate) tools: ToolRegistry,
    pub(crate) jobs: Option<Arc<dyn joyczl_tools::jobs::JobRegistry>>,
    /// See `Server::exec_session_allow` — same `Arc` the exec policy holds.
    pub(crate) exec_session_allow: Arc<Mutex<Vec<String>>>,
    /// See `Server::write_approved_dirs` — same `Arc` the write policy holds.
    pub(crate) write_approved_dirs: Arc<Mutex<Vec<PathBuf>>>,
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

pub(crate) async fn builtin_tools(
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
