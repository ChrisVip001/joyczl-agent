//! `joyczl-ops` —— 驾驶舱的 HTTP 面：axum + SSE。
//!
//! 它是 `joy app-server` 的**客户端**，跟 CLI、TS 网关、Python SDK 一样。
//! 唯一特殊的地方是它把协议翻译成浏览器看得懂的东西：
//!
//! ```text
//!   GET  /api/data     四个读方法 → 一屏数据（DashboardData）
//!   GET  /api/session  session/messages → 一个会话说过的话（一页）
//!   POST /api/turn     turn/start → 一条 SSE 流（ServerNotification 的 JSON）
//!   GET  /*            joy-ts/packages/dashboard/dist 里的静态文件
//! ```
//!
//! **为什么不在这里直接打开 state.db**：那会多出第二个持有 state.db 的
//! 进程 —— 整个架构都在躲这件事（README「state.db 只活在一个进程里」）。
//! 驾驶舱看得见的一切都从协议里来，所以它看到的世界跟别人看到的是同一个。

mod api;
mod client;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::http::{header, HeaderValue};
use axum::routing::{get, post};
use axum::Router;
use joyczl_config::Settings;
use joyczl_protocol::{ErrorNotification, ErrorObject, ServerNotification};
use tokio::net::TcpListener;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

pub use client::AppServer;

/// 驾驶舱的默认端口（`plan.md` 里写死的那一个）。
pub const DEFAULT_PORT: u16 = 7777;
/// 默认端口被占用时往后试几个。
const PORT_SCAN: u16 = 10;

/// 起驾驶舱，直到进程被杀掉。
pub async fn serve(settings: Settings) -> Result<()> {
    if !settings.home.exists() {
        eprintln!(
            "(joy) 状态目录 {} 还不存在，app-server 会建它",
            settings.home.display()
        );
    }

    let app = Arc::new(AppServer::spawn(&settings.home)?);
    let assets = assets_dir();
    let router = router(Arc::clone(&app), assets.clone());
    let first = port();

    for candidate in first..first + PORT_SCAN {
        // 只绑 127.0.0.1：这是个人助理的仪表盘，不是内网服务。
        // 想从别的机器看，那是另一个决定（要带鉴权），不该是默认。
        match TcpListener::bind(("127.0.0.1", candidate)).await {
            Ok(listener) => {
                println!("Joy 驾驶舱 → http://localhost:{candidate}");
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
        "127.0.0.1 上 {first}–{} 都占着。用 JOY_PORT 指定一个空端口。",
        first + PORT_SCAN - 1
    )
}

/// 路由表。抽出来是为了测试能直接打它，不必真起监听。
pub fn router(app: Arc<AppServer>, assets: PathBuf) -> Router {
    Router::new()
        .route("/api/data", get(api::data))
        .route("/api/session", get(api::session))
        .route("/api/turn", post(api::turn))
        .route("/api/approval", post(api::approval))
        .route("/api/goal", post(api::goal))
        // 静态文件兜底。`ServeDir` 而不是自己读文件：路径穿越（`/../../.env`）
        // 是那种已经被写对过无数次的东西，没有理由再写一次。
        // `append_index_html_on_directories` 让 `/` 落到 index.html。
        .fallback_service(ServeDir::new(assets).append_index_html_on_directories(true))
        // 不缓存：前端一直在改，浏览器缓存会把「我明明改了」变成十分钟的鬼故事。
        // 对 /api/* 也一并生效 —— 那些响应本来就不该被缓存。
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
        .with_state(app)
}

/// 把「应答里的错误」装成「流里的一条通知」。
///
/// 两个类型字段一模一样，分成两个只是因为 JSON-RPC 的信封不同（一个是
/// `error`，一个是 `turn/notification` 的参数）。转一下，浏览器那边就
/// 只需要认一种形状：不管是这一轮跑出来的错，还是这一轮压根没起来，
/// 收到的都是一个 `type: "error"` 的通知。
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

/// 前端构建产物的位置。
///
/// 默认值由**编译期**的 `CARGO_MANIFEST_DIR` 推出来，指向源码树里的
/// `joy-ts/packages/dashboard/dist`。这是刻意的：`joy dashboard` 是开发期的
/// 命令，前端还在天天改 —— 改完重跑一次 `just dashboard-build`（几十毫秒）
/// 刷新就能看见，**不用重编 Rust、不用重启服务**。嵌进二进制就没这个了。
/// 真要分发的那天再说分发的事（那时是 rust-embed，改的也只有这一个函数）。
///
/// `JOY_DASHBOARD_DIR` 优先，留给「我就想让它指别处」的场合。
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
