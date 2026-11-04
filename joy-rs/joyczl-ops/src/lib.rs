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
//! **Why state.db is not opened directly here**: that would add a second process holding
//! state.db — the whole architecture avoids that (README: "state.db lives in one process").
//! Everything the dashboard sees comes from the protocol, so it sees the same world.

mod api;
mod client;
mod freshness;
pub mod hub;

#[cfg(test)]
#[path = "router_tests.rs"]
mod router_tests;

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
            "(joy) 状态目录 {} 还不存在，app-server 会建它",
            settings.home.display()
        );
    }

    let app = Arc::new(AppServer::spawn(&settings.home)?);
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
                        "Joy 驾驶舱 → http://{bind_host}:{candidate}（/api/* 需要 Bearer token）"
                    ),
                    None => println!("Joy 驾驶舱 → http://{bind_host}:{candidate}（本机免鉴权）"),
                }
                if !assets.join("index.html").exists() {
                    eprintln!(
                        "(joy) 前端还没构建（{}）。先跑：just dashboard-build",
                        assets.display()
                    );
                }
                return axum::serve(listener, router)
                    .await
                    .context("HTTP 服务异常退出");
            }
            Err(error) => println!("端口 {candidate} 用不了（{error}），换一个"),
        }
    }

    anyhow::bail!(
        "{bind_host} 上 {first}–{} 都占着。用 JOY_PORT 指定一个空端口。",
        first + PORT_SCAN - 1
    )
}

/// Multi-user mode: one ops process serves every user under `<root>`.
/// Auth is the token in users.json — the same Bearer path as single-user mode.
pub async fn serve_hub(root: &Path, host: &str, max_open: usize, idle_secs: u64) -> Result<()> {
    let hub = Arc::new(hub::Hub::open_with(root, max_open, idle_secs)?);
    eprintln!(
        "(joy) hub：{} 个用户，根目录 {}，池上限 {} 个实例，闲置 {} 秒关",
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
                    "Joy hub → http://{bind_host}:{candidate}（{} 个用户，各自的 home 在 {}）",
                    hub.user_count(),
                    root.display()
                );
                return axum::serve(listener, router)
                    .await
                    .context("HTTP 服务异常退出");
            }
            Err(error) => println!("端口 {candidate} 用不了（{error}），换一个"),
        }
    }
    anyhow::bail!(
        "{bind_host} 上 {first}–{} 都占着。用 JOY_PORT 指定一个空端口。",
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
                eprintln!("(joy) 用户 {user} 的 app-server 起不来：{error:#}");
                return (
                    axum::http::StatusCode::BAD_GATEWAY,
                    format!("该用户的 app-server 起不来：{error:#}"),
                )
                    .into_response();
            }
        }
    } else {
        gate.single
            .clone()
            .expect("单用户模式的 Gate 必须带一个 app-server")
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
            "驾驶舱要绑在 {host}（非回环地址）却没设 JOY_OPS_TOKEN —— 拒绝启动。\
             设一个 token，或把 JOY_OPS_HOST 留在 127.0.0.1。"
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
        .route("/api/session", get(api::session))
        .route("/api/turn", post(api::turn))
        .route("/api/approval", post(api::approval))
        .route("/api/interrupt", post(api::interrupt))
        .route("/api/goal", post(api::goal))
        .route("/api/usage", get(api::usage))
        .route("/api/approvals", get(api::approvals))
        .route("/api/schedules", get(api::schedules))
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
        // The token layer only covers the /api/* routes above (route_layer does not affect
        // the fallback static files): static assets carry no data; the page sends the token.
        .route_layer(axum::middleware::from_fn_with_state(
            gate.clone(),
            authorize,
        ))
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
