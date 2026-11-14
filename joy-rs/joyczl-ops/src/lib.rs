//! `joyczl-ops` — the HTTP face of the dashboard: axum + SSE.
//!
//! It is a **client** of `joy app-server`, like the CLI, the TS gateway and the Python SDK.
//! The only special thing: it translates the protocol into what a browser understands:
//!
//! ```text
//!   GET  /api/data     the four read methods → one screen of data (DashboardData)
//!   GET  /api/session  session/messages → what one session said (one page)
//!   POST /api/turn     turn/start → one SSE stream (JSON of ServerNotification)
//!   GET  /*            static files from joy-ts/packages/dashboard/dist
//! ```
//!
//! The dashboard reads runtime-owned state through the protocol. It must not create a
//! competing execution owner or run recovery. Every client observes the same runtime.

mod api;
mod auth;
mod capabilities;
mod client;
mod freshness;
pub mod hub;
mod memory_api;
mod schedules;
mod webhooks;

#[cfg(test)]
#[path = "auth_tests.rs"]
mod auth_tests;

#[cfg(test)]
#[path = "capabilities_tests.rs"]
mod capabilities_tests;

#[cfg(test)]
#[path = "events_tests.rs"]
mod events_tests;

#[cfg(test)]
#[path = "router_tests.rs"]
mod router_tests;

#[cfg(test)]
#[path = "webhook_tests.rs"]
mod webhook_tests;

#[cfg(test)]
#[path = "hub_tests.rs"]
mod hub_tests;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::http::{header, HeaderValue};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use joyczl_config::Settings;
use joyczl_protocol::{ErrorNotification, ErrorObject, ServerNotification};
use tokio::net::TcpListener;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

pub use client::AppServer;

/// Default port of the dashboard (the one hardcoded in `plan.md`).
pub const DEFAULT_PORT: u16 = 7777;
/// How many ports to scan forward when the default port is taken.
const PORT_SCAN: u16 = 10;

/// Start the dashboard, until the process is killed.
pub async fn serve(settings: Settings) -> Result<()> {
    // Binding validation happens **before any port is opened**: non-loopback
    // addresses must come with a token. The check lives in startup code, not
    // docs — reject wildcard listeners at load: misconfig must fail to start.
    validate_binding(&settings.ops_host, settings.ops_token.as_deref())?;

    if !settings.home.exists() {
        eprintln!(
            "(joy) the state directory {} does not exist yet; the app-server will create it",
            settings.home.display()
        );
    }

    let app = Arc::new(AppServer::spawn(&settings.home, true)?);
    let assets = assets_dir();
    if let Err(why) = freshness::check_dashboard_freshness(&assets) {
        anyhow::bail!("{why}");
    }
    let router = router(
        Gate::single(Arc::clone(&app), settings.ops_token.clone()),
        assets.clone(),
    );
    let first = port();
    let bind_host = bind_addr(&settings.ops_host);

    for candidate in first..first + PORT_SCAN {
        match TcpListener::bind((bind_host, candidate)).await {
            Ok(listener) => {
                match settings.ops_token.as_deref() {
                    Some(_) => println!(
                        "Joy cockpit → http://{bind_host}:{candidate} (/api/* needs a Bearer token)"
                    ),
                    None => println!(
                        "Joy cockpit → http://{bind_host}:{candidate} (no auth on this machine)"
                    ),
                }
                if !assets.join("index.html").exists() {
                    eprintln!(
                        "(joy) the front end has not been built ({}). Run: just dashboard-build",
                        assets.display()
                    );
                }
                return axum::serve(listener, router)
                    .await
                    .context("the HTTP server exited unexpectedly");
            }
            Err(error) => println!("port {candidate} is unusable ({error}); trying the next one"),
        }
    }

    anyhow::bail!(
        "{first}–{} are all taken on {bind_host}. Pick a free port with JOY_PORT.",
        first + PORT_SCAN - 1
    )
}

/// Multi-user mode: one ops process serves every user under `<root>`.
/// Auth is the token in users.json — the same Bearer path as single-user mode.
pub async fn serve_hub(root: &Path, host: &str, max_open: usize, idle_secs: u64) -> Result<()> {
    let hub = Arc::new(hub::Hub::open_with(root, max_open, idle_secs)?);
    // First-user policy: a fresh hub on a public bind must not let a stranger claim it —
    // the owner registers from loopback or via `joy hub adduser`.
    hub.permit_first_registration(bind_addr(host).is_loopback());
    eprintln!(
        "(joy) hub: {} users, root {}, pool capped at {} instances, idle ones closed after {} seconds",
        hub.user_count(),
        root.display(),
        max_open,
        idle_secs
    );
    let assets = assets_dir();
    if let Err(why) = freshness::check_dashboard_freshness(&assets) {
        anyhow::bail!("{why}");
    }
    let router = router(Gate::hub(Arc::clone(&hub)), assets.clone());
    let first = port();
    let bind_host = bind_addr(host);

    for candidate in first..first + PORT_SCAN {
        match TcpListener::bind((bind_host, candidate)).await {
            Ok(listener) => {
                println!(
                    "Joy hub → http://{bind_host}:{candidate} ({} users, each home under {})",
                    hub.user_count(),
                    root.display()
                );
                return axum::serve(listener, router)
                    .await
                    .context("the HTTP server exited unexpectedly");
            }
            Err(error) => println!("port {candidate} is unusable ({error}); trying the next one"),
        }
    }
    anyhow::bail!(
        "{first}–{} are all taken on {bind_host}. Pick a free port with JOY_PORT.",
        first + PORT_SCAN - 1
    )
}

/// Request gate: static token (single user, `JOY_OPS_TOKEN`) + user resolution (hub mode).
/// Both modes go through the **same Bearer path** — semantics do not fork by mode.
#[derive(Clone)]
pub struct Gate {
    token: Option<String>,
    hub: Option<Arc<hub::Hub>>,
    single: Option<Arc<AppServer>>,
}

impl Gate {
    /// Single-user mode: one app-server + optional static token.
    pub fn single(app: Arc<AppServer>, token: Option<String>) -> Self {
        Self {
            token,
            hub: None,
            single: Some(app),
        }
    }

    /// Multi-user mode: token mandatory (users.json), app-server lazy-loaded per user.
    pub fn hub(hub: Arc<hub::Hub>) -> Self {
        Self {
            token: None,
            hub: Some(hub),
            single: None,
        }
    }

    /// The hub, when this is hub mode. The auth endpoints live *outside* the authorize
    /// layer, so they reach the user table through this instead of an extension.
    pub(crate) fn hub_ref(&self) -> Option<&Arc<hub::Hub>> {
        self.hub.as_ref()
    }

    /// The single-user app-server. Unauthenticated routes (inbound webhooks) need it too:
    /// their proof is the hook's own secret, not the dashboard token — and in hub mode there
    /// is no user to pick, so the door simply does not exist there.
    pub(crate) fn single_app(&self) -> Option<Arc<AppServer>> {
        self.single.clone()
    }
}

async fn authorize(
    axum::extract::State(gate): axum::extract::State<Gate>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    // 1) Static token: when configured, **everyone must carry it** (no local bypass — otherwise
    //    the token only stops strangers, not you). route_layer keeps this on /api/* only.
    if let Some(expected) = &gate.token {
        let provided = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        if !provided.is_some_and(|t| constant_time_eq(t.as_bytes(), expected.as_bytes())) {
            return axum::http::StatusCode::UNAUTHORIZED.into_response();
        }
    }

    // 2) User resolution + lazy-load that user's app-server. Hub mode: no token, no user.
    let app = if let Some(hub) = &gate.hub {
        let auth = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        let Some(user) = hub.resolve(auth) else {
            return axum::http::StatusCode::UNAUTHORIZED.into_response();
        };
        match hub.client(&user).await {
            Ok(app) => app,
            Err(error) => {
                // Never silent: say which user failed and why.
                eprintln!("(joy) could not start the app-server for user {user}: {error:#}");
                return (
                    axum::http::StatusCode::BAD_GATEWAY,
                    format!("the app-server for that user would not start: {error:#}"),
                )
                    .into_response();
            }
        }
    } else {
        gate.single
            .clone()
            .expect("single-user mode's Gate must carry an app-server")
    };
    req.extensions_mut().insert(app);
    next.run(req).await
}

/// Binding validation: non-loopback addresses require a token (nanobot's "0.0.0.0 must set
/// api_key" rule). Loopback + token = authenticated even locally; allowed (multi-user step 1).
pub fn validate_binding(host: &str, token: Option<&str>) -> Result<()> {
    let loopback = match host {
        "localhost" => true,
        other => other
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false),
    };
    if !loopback && token.is_none() {
        anyhow::bail!(
            "the cockpit would bind to {host} (not a loopback address) with no JOY_OPS_TOKEN — refusing to start.\
             Set a token, or leave JOY_OPS_HOST at 127.0.0.1."
        );
    }
    Ok(())
}

/// IP to bind: `localhost` resolves to 127.0.0.1 (TcpListener does not accept hostnames).
fn bind_addr(host: &str) -> std::net::IpAddr {
    match host {
        "localhost" => std::net::IpAddr::from([127, 0, 0, 1]),
        other => other
            .parse()
            .unwrap_or(std::net::IpAddr::from([127, 0, 0, 1])),
    }
}

/// Constant-time compare: return at once on length mismatch (length is not secret); content
/// accumulates XOR per byte — bit-level short-circuit would leak "which byte was wrong".
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |diff, (x, y)| diff | (x ^ y))
        == 0
}

/// Route table. Pulled out so tests can hit it directly without starting a listener.
pub fn router(gate: Gate, assets: PathBuf) -> Router {
    Router::new()
        .route("/api/data", get(api::data))
        // Cross-device sync: lightweight pings (see EventPing) — other devices learn
        // "something happened", pull the details themselves.
        .route("/api/events", get(api::events))
        .route("/api/session", get(api::session))
        // A session exists before its first message (0.15): create it with a name + working dir.
        .route("/api/session/meta", get(api::session_meta))
        .route("/api/session/new", post(api::session_new))
        .route("/api/session/update", post(api::session_update))
        .route("/api/session/fork", post(api::session_fork))
        // What is running right now — the page cannot know this from its own `busy` flag,
        // because another device may be the one running the turn.
        .route("/api/turns", get(api::turns))
        .route("/api/turn", post(api::turn))
        // Add to the turn in flight instead of starting a second one (0.16).
        .route("/api/turn/steer", post(api::steer))
        // "When you're done": queued as its own turn, drained when the current one ends.
        .route("/api/turn/follow-up", post(api::follow_up))
        .route("/api/approval", post(api::approval))
        .route("/api/interrupt", post(api::interrupt))
        // Long tasks: list + one door for create/suspend/resume/cancel (human-only switches).
        .route("/api/tasks", get(api::tasks))
        .route("/api/task", post(api::task))
        .route("/api/turn-operation", post(api::turn_operation))
        // Inbound mail: the human's "handle these".
        .route("/api/inbox", post(api::inbox))
        .route("/api/usage", get(api::usage))
        .route("/api/approvals", get(api::approvals))
        .route("/api/schedules", get(api::schedules))
        // The capability center's third list: declarations (file + skills) with runtime state,
        // and the two writes a human owns (`schedules.json` only — skill jobs stay read-only).
        .route("/api/schedules/jobs", get(api::schedule_jobs))
        .route("/api/schedules/save", post(api::schedule_save))
        .route("/api/schedules/remove", post(api::schedule_remove))
        // File upload: single octet-stream file, 20MB cap (axum default 2MB; raised explicitly).
        .route("/api/upload", post(api::upload))
        // Directory browsing: for the dashboard to pick a working directory (read-only, dirs).
        .route("/api/fs", get(api::fs_browse))
        .route("/api/fs/mkdir", post(api::fs_mkdir))
        .route(
            "/api/fs/recents",
            get(api::fs_recents).post(api::fs_recents_record),
        )
        // Multi-model: list / save-and-switch / remove. Same implementation as terminal /model.
        .route("/api/model/profiles", get(api::model_profiles))
        .route("/api/model/save", post(api::model_save))
        .route("/api/model/remove", post(api::model_remove))
        // The routing table: fixed / auto, and which profile each task tier means.
        .route("/api/model/routes", get(api::model_routes))
        .route("/api/model/routes/save", post(api::model_routes_save))
        // The capability center: skills and MCP connectors. Built-in rows (JOY_SKILL_DIRS /
        // JOY_MCP_CONFIG) are listed but every write refuses them — the user's own files
        // are the only ones a browser may touch.
        .route("/api/skills", get(api::skills))
        .route("/api/skills/install", post(api::skills_install))
        .route("/api/skills/remove", post(api::skills_remove))
        .route("/api/mcp", get(api::mcp))
        .route("/api/mcp/save", post(api::mcp_save))
        .route("/api/mcp/remove", post(api::mcp_remove))
        // The memory center: ledger, memory pages, the context meter, skill hits — all
        // read-only — plus `/api/compact`, the human's own compaction button.
        .route("/api/ledger/turns", get(memory_api::ledger_turns))
        .route("/api/ledger/turn", get(memory_api::ledger_turn))
        .route("/api/memory/index", get(memory_api::memory_index))
        .route("/api/memory/page", get(memory_api::memory_page))
        .route("/api/context", get(memory_api::context))
        .route("/api/skills/stats", get(memory_api::skill_stats))
        .route("/api/compact", post(memory_api::compact))
        // The task API: one turn in, one JSON answer out (no SSE) — for programs, not people.
        .route("/api/run", post(api::run))
        // The token layer only covers the /api/* routes above (route_layer does not affect
        // the fallback static files): static assets carry no data; the page sends the token.
        .route_layer(axum::middleware::from_fn_with_state(
            gate.clone(),
            authorize,
        ))
        // Account endpoints — registered *after* the authorize layer, so they are the
        // only /api/* reachable without a token. That is the whole point of login.
        .route("/api/auth/register", post(crate::auth::register))
        .route("/api/auth/login", post(crate::auth::login))
        // Inbound webhooks — also after the token layer, on purpose: the caller is a foreign
        // machine that proves itself with the hook's secret (`X-Joy-Secret`), never the dashboard
        // token. Unknown/disabled names 404, a wrong secret 401 (see `webhooks.rs`).
        .route("/api/hook/{name}", post(api::hook))
        // Static file fallback. `ServeDir` instead of reading files ourselves: path traversal
        // (`/../../.env`) is the kind of thing written correctly countless times — don't rewrite
        // it. `append_index_html_on_directories` makes `/` land on index.html.
        .fallback_service(ServeDir::new(assets).append_index_html_on_directories(true))
        // No caching: the frontend changes constantly; browser caching turns "I did change it"
        // into a ten-minute ghost story. Also applies to /api/* — those must never be cached.
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
        // Cross-origin clients (Tauri shell, a PWA served elsewhere) talk to this API with
        // a Bearer header — never cookies — so a permissive CORS layer adds no new attack
        // surface: the browser does not attach credentials on its own.
        .layer(tower_http::cors::CorsLayer::permissive())
        .layer(axum::extract::DefaultBodyLimit::max(25 * 1024 * 1024))
        .with_state(gate)
}

/// Wrap "an error in the response" as "a notification in the stream".
///
/// The two types have identical fields; separate only because the JSON-RPC envelopes differ
/// (one is `error`, the other is `turn/notification`'s params). After the swap the browser
/// only recognizes one shape: turn failed mid-run or never started — either way it receives
/// a notification with `type: "error"`.
pub(crate) fn as_notification(error: ErrorObject) -> ServerNotification {
    ServerNotification::Error(ErrorNotification {
        code: error.code,
        message: error.message,
        data: error.data,
    })
}

fn port() -> u16 {
    std::env::var("JOY_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

/// Where the frontend build output lives.
///
/// The default is derived at **compile time** from `CARGO_MANIFEST_DIR`, pointing at
/// `joy-ts/packages/dashboard/dist` in the source tree. Deliberate: `joy dashboard` is a
/// dev-time command, the frontend changes daily — rerun `just dashboard-build` (tens of
/// ms) and a refresh shows it, **no Rust rebuild, no server restart**. Embedding in the
/// binary would lose that; distribution day would mean rust-embed, changing only this fn.
///
/// `JOY_DASHBOARD_DIR` takes precedence, for "I want it to point elsewhere".
pub fn assets_dir() -> PathBuf {
    match std::env::var_os("JOY_DASHBOARD_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => workspace_assets(),
    }
}

fn workspace_assets() -> PathBuf {
    // joy-rs/joyczl-ops → joyczl-agent → joy-ts/packages/dashboard/dist
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("joy-ts")
        .join("packages")
        .join("dashboard")
        .join("dist")
}
