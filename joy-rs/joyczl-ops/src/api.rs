//! 驾驶舱的三个端点。
//!
//! 只有三个，因为驾驶舱该做的事就这么三件：**看个大概**、**翻旧账**、**说话**。
//! 一切都是从 app-server 的读方法和 `turn/start` 翻过来的，
//! 这里不新增任何业务 —— 新增业务的地方在协议那一侧。

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Extension, Query};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Local;
use futures_util::stream::{self, Stream};
use joyczl_protocol::{
    codes, methods, ApprovalLogResponse, ApprovalRespondParams, ApprovalRespondResponse,
    ConfigReadResponse, DashboardData, ErrorObject, GoalSetParams, GoalSetResponse,
    MemoryListEpisodesResponse, MemoryListResponse, ModelProfilesResponse, ModelRemoveParams,
    ModelRemoveResponse, ModelSaveParams, ModelSaveResponse, ScheduleListResponse,
    ServerNotification, SessionListResponse, SessionMessagesParams, SessionMessagesResponse,
    TurnStartParams, TurnStartResponse, UsageSummaryResponse,
};
use serde_json::json;
use tokio::sync::{broadcast, mpsc};

use crate::AppServer;

/// 首屏只取最近这么多条。驾驶舱不是数据库客户端 —— 它要让人一眼看到
/// 「现在是什么状态」，翻页是另一件事（协议里的 cursor 就是为那天留的）。
const SESSION_LIMIT: u32 = 20;
const FACT_LIMIT: u32 = 30;
const EPISODE_LIMIT: u32 = 20;

/// 还没被浏览器读走的通知最多攒多少。用**无界**通道是有意的：
/// 有界的话，浏览器读得慢就会让转发任务卡在 send 上，从而来不及消费
/// 广播通道 —— 中间那些 `textDelta` 会被丢掉，还可能把 `turnCompleted`
/// 一起丢掉，那一轮 SSE 就永远不结束了。无界的上限是一整轮对话的通知量，
/// 一轮结束通道就跟着销毁。
const TURN_BUFFER: usize = 1024;

/// `GET /api/data` —— 首屏要的一切，一次取齐。
///
/// 合成一个端点而不是让前端并发发四个请求：app-server 一次只处理一条
/// 请求（stdio 上就是一个循环），四个并发请求到了那儿还是排队。合成一个
/// 还顺带给了前端一个「这些数字是同一时刻的」保证。
pub async fn data(Extension(app): Extension<Arc<AppServer>>) -> Response {
    match collect(&app).await {
        Ok(data) => Json(data).into_response(),
        Err(error) => failed(StatusCode::BAD_GATEWAY, error),
    }
}

async fn collect(app: &AppServer) -> Result<DashboardData, ErrorObject> {
    let config: ConfigReadResponse = app.request(methods::CONFIG_READ, json!({})).await?;
    let sessions: SessionListResponse = app
        .request(methods::SESSION_LIST, json!({ "limit": SESSION_LIMIT }))
        .await?;
    let facts: MemoryListResponse = app
        .request(methods::MEMORY_LIST, json!({ "limit": FACT_LIMIT }))
        .await?;
    let episodes: MemoryListEpisodesResponse = app
        .request(
            methods::MEMORY_LIST_EPISODES,
            json!({ "limit": EPISODE_LIMIT }),
        )
        .await?;

    Ok(DashboardData {
        generated_at: Local::now().to_rfc3339(),
        config: config.config,
        sessions: sessions.data,
        facts: facts.data,
        episodes: episodes.data,
    })
}

/// `GET /api/session?sessionId=…&cursor=…&limit=…` —— 一个会话说过的话。
///
/// 为什么它不在 `/api/data` 里：那一屏是**概览**，每 15 秒整个重刷一次，
/// 说的是「现在什么状态」。而这是一段**历史** —— 按会话取、按页往上翻，
/// 翻页的时候不该连配置和记忆一起重取。
///
/// 查询参数直接用协议里的 `SessionMessagesParams`（**没有第二个形状**），
/// 所以 URL 上就是 `sessionId` / `cursor` / `limit` 这三个名字；哪个是必填的
/// 也由那个类型说了算，不在这儿重新写一遍。
pub async fn session(
    Extension(app): Extension<Arc<AppServer>>,
    Query(params): Query<SessionMessagesParams>,
) -> Response {
    let payload = serde_json::to_value(&params).expect("SessionMessagesParams 一定能序列化");
    match app
        .request::<SessionMessagesResponse>(methods::SESSION_MESSAGES, payload)
        .await
    {
        Ok(page) => Json(page).into_response(),
        Err(error) => failed(StatusCode::BAD_GATEWAY, error),
    }
}

/// `POST /api/approval` —— 回答一次「要不要执行」。
/// `POST /api/goal` —— 设或清一个目标（`condition` 缺省/为空 = 清除）。
///
/// 与 turn 走同一条路：app-server 是子进程，`goal/set` 也是它的一个方法。
/// **驾驶舱不是唯一入口**：终端里的 `/goal` 与它是同一件事。
pub async fn goal(
    Extension(app): Extension<Arc<AppServer>>,
    Json(params): Json<GoalSetParams>,
) -> Response {
    let payload = serde_json::to_value(&params).expect("GoalSetParams 一定能序列化");
    match app
        .request::<GoalSetResponse>(methods::GOAL_SET, payload)
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

///
/// 载荷是协议里的 `ApprovalRespondParams`。**不是 SSE**：这是一问一答，
/// 答完就没了。太晚送达时返回 `accepted: false`（那一轮早已不等了），
/// 前端据此知道自己的点击没人听，而不是以为批准生效了。
/// 已配置的多套模型 + 当前那套（**不含密钥**，只有「key 存在哪」的指针）。
///
/// 与 `/api/goal` 同一条纪律：这些是**只有人能改**的东西 —— 模型没有
/// `model/*` 工具，也拿不到密钥。
/// 用量台账（迁移 0010）：per-model 聚合 + 总数。只记账不强制。
pub async fn usage(Extension(app): Extension<Arc<AppServer>>) -> Response {
    match app
        .request::<UsageSummaryResponse>(methods::USAGE_SUMMARY, json!({}))
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// 审批审计（迁移 0011）：最近 100 条「问了才跑」，新的在前。
pub async fn approvals(Extension(app): Extension<Arc<AppServer>>) -> Response {
    match app
        .request::<ApprovalLogResponse>(methods::APPROVAL_LOG, json!({}))
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// 调度状态快照（迁移 0012）：每任务的 last_fired/claimed/catch_up/delivery。
pub async fn schedules(Extension(app): Extension<Arc<AppServer>>) -> Response {
    match app
        .request::<ScheduleListResponse>(methods::SCHEDULE_LIST, json!({}))
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// 单个上传文件的上限（openhuman 的预算表）：20MB。
const UPLOAD_MAX_BYTES: usize = 20 * 1024 * 1024;

/// 扩展名白名单：1-8 位字母数字。双扩展、路径段、奇怪字符都不进路径。
fn safe_extension(name: &str) -> Option<String> {
    let ext = std::path::Path::new(name).extension()?.to_str()?;
    let ok = !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric());
    ok.then(|| ext.to_ascii_lowercase())
}

/// `POST /api/upload?name=<原始文件名>` —— body 是文件原始字节。
///
/// 通道选 octet-stream 而不是 multipart：单文件用不着 multipart 的信封，
/// 还避开了解析器这个安全雷区（零 feature、零传递依赖）。
/// 落盘名**完全由服务端生成**（spill 同款纪律：用户输入进不了路径），
/// 原始名只取扩展名过白名单。返回相对 home 的路径 —— 前端把它注进消息，
/// 模型用 read_file 自己读（pi/codex 的「引用优先」：引用与读取解耦）。
pub async fn upload(
    Extension(app): Extension<Arc<AppServer>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > UPLOAD_MAX_BYTES {
        return failed(
            StatusCode::PAYLOAD_TOO_LARGE,
            ErrorObject {
                code: codes::INVALID_PARAMS,
                message: format!("文件超过 20MB 上限（{} 字节）", body.len()),
                data: None,
            },
        );
    }
    if body.is_empty() {
        return failed(
            StatusCode::BAD_REQUEST,
            ErrorObject {
                code: codes::INVALID_PARAMS,
                message: "空文件不收。".into(),
                data: None,
            },
        );
    }
    let ext = params
        .get("name")
        .and_then(|n| safe_extension(n))
        .unwrap_or_else(|| "bin".to_string());
    // home 走协议取（AppServer 是子进程句柄，没有本地字段）——上传是低频操作，
    // 多一次往返无所谓，换来的是 ops 层继续保持「只通过协议说话」的纪律。
    let config: ConfigReadResponse = match app.request(methods::CONFIG_READ, json!({})).await {
        Ok(config) => config,
        Err(error) => return failed(StatusCode::BAD_GATEWAY, error),
    };
    let home = std::path::PathBuf::from(config.config.home);
    let now = Local::now();
    let dir = home.join("uploads").join(now.format("%Y%m%d").to_string());
    if let Err(why) = tokio::fs::create_dir_all(&dir).await {
        return failed(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorObject {
                code: codes::INTERNAL_ERROR,
                message: format!("建目录失败：{why}"),
                data: None,
            },
        );
    }
    // 名字里只有服务端生成的成分：时间戳 + 纳秒后缀 —— 撞名不可能，覆盖不了旧文件。
    let name = format!(
        "file-{}-{:x}.{ext}",
        now.format("%H%M%S"),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    );
    let full = dir.join(&name);
    if let Err(why) = tokio::fs::write(&full, &body).await {
        return failed(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorObject {
                code: codes::INTERNAL_ERROR,
                message: format!("写文件失败：{why}"),
                data: None,
            },
        );
    }
    let date = now.format("%Y%m%d");
    Json(json!({ "path": format!("uploads/{date}/{name}") })).into_response()
}

/// 目录浏览的应答。ops 本地的 UI 视图（与 /api/upload 的应答同一待遇）：
/// 不进协议 —— SDK 与终端都用不到「浏览服务端目录」这件事。
#[derive(serde::Serialize)]
pub struct FsListing {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<FsEntry>,
}

#[derive(serde::Serialize)]
pub struct FsEntry {
    pub name: String,
    pub path: String,
}

/// 列一个目录下的**子目录**（只读、只列目录 —— 选工作目录用不到文件）。
/// 独立成纯函数是为了能直接单测，不必起一个 app-server。
pub fn list_dirs(raw: &str, include_hidden: bool) -> Result<FsListing, String> {
    let path = std::path::PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(format!("'{raw}' 必须是绝对路径"));
    }
    if !path.is_dir() {
        return Err(format!("'{raw}' 不存在或不是目录"));
    }
    let mut entries: Vec<FsEntry> = std::fs::read_dir(&path)
        .map_err(|why| format!("读不动 {raw}：{why}"))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let is_dir = entry.file_type().ok()?.is_dir();
            if !is_dir {
                return None;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            // 点开头默认藏（dsh 的 hidden 旗标语义），开了开关才露出；
            // 构建噪音（node_modules/target）无论开关都藏 —— 点进去没有意义。
            if (name.starts_with('.') && !include_hidden)
                || name == "node_modules"
                || name == "target"
            {
                return None;
            }
            Some(FsEntry {
                path: entry.path().to_string_lossy().to_string(),
                name,
            })
        })
        .collect();
    entries.sort_by_key(|e| e.name.to_lowercase());
    Ok(FsListing {
        path: path.to_string_lossy().to_string(),
        parent: path
            .parent()
            .filter(|p| p.as_os_str() != path.as_os_str())
            .map(|p| p.to_string_lossy().to_string()),
        entries,
    })
}

/// 浏览的起点：用户主目录，退化到进程 cwd。
fn default_browse_root() -> String {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .or_else(|_| std::env::current_dir().map(|p| p.to_string_lossy().to_string()))
        .unwrap_or_else(|_| "/".to_string())
}

/// `GET /api/fs?path=<目录>` —— 服务端目录浏览（驾驶舱选工作目录用）。
///
/// 为什么在服务端：浏览器的文件夹选择器**拿不到绝对路径**（File System
/// Access API 只给句柄不给路径），而工作目录恰恰需要绝对路径 —— 所以
/// 「像桌面应用那样选文件夹」必须由知道文件系统的这一侧来做
/// （Jupyter、VS Code server 都是同款做法）。只读、只列目录。
pub async fn fs_browse(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let raw: String = params
        .get("path")
        .map(String::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(default_browse_root);
    let include_hidden = params
        .get("hidden")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    match list_dirs(&raw, include_hidden) {
        Ok(listing) => Json(listing).into_response(),
        Err(why) => failed(
            StatusCode::BAD_REQUEST,
            ErrorObject {
                code: codes::INVALID_PARAMS,
                message: why,
                data: None,
            },
        ),
    }
}

/// 在 parent 下建一个子目录，返回新目录的绝对路径（dsh 的 createDirectory
/// verb）。独立成纯函数是为了能直接单测。
///
/// 名字纪律与 upload 的扩展名白名单同一条：**用户输入进不了路径形状** ——
/// 不许分隔符、不许 `..`、不许点开头（建出来的目录不该默认隐身）、
/// 长度封顶 64。
pub fn create_subdir(parent_raw: &str, name: &str) -> Result<String, String> {
    let parent = std::path::PathBuf::from(parent_raw);
    if !parent.is_absolute() {
        return Err(format!("父目录 '{parent_raw}' 必须是绝对路径"));
    }
    if !parent.is_dir() {
        return Err(format!("父目录 '{parent_raw}' 不存在或不是目录"));
    }
    let name = name.trim();
    let bad = |why: &str| format!("目录名 '{name}' 不合格：{why}");
    if name.is_empty() || name.len() > 64 {
        return Err(bad("1-64 个字符"));
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") || name.starts_with('.') {
        return Err(bad("不能包含路径形状（分隔符、..、点开头）"));
    }
    let target = parent.join(name);
    if target.exists() {
        return Err(format!("'{name}' 在 {parent_raw} 下已经存在"));
    }
    std::fs::create_dir(&target).map_err(|why| format!("建不动：{why}"))?;
    Ok(target.to_string_lossy().to_string())
}

/// `POST /api/fs/mkdir?path=<父目录>&name=<目录名>` —— 选择器里的
/// 「新建文件夹」。只读浏览之外唯一的一个写动作，名字过白名单。
pub async fn fs_mkdir(Query(params): Query<std::collections::HashMap<String, String>>) -> Response {
    let parent = params.get("path").cloned().unwrap_or_default();
    let name = params.get("name").cloned().unwrap_or_default();
    match create_subdir(&parent, &name) {
        Ok(created) => Json(json!({ "path": created })).into_response(),
        Err(why) => failed(
            StatusCode::BAD_REQUEST,
            ErrorObject {
                code: codes::INVALID_PARAMS,
                message: why,
                data: None,
            },
        ),
    }
}

pub async fn model_profiles(Extension(app): Extension<Arc<AppServer>>) -> Response {
    match app
        .request::<ModelProfilesResponse>(methods::MODEL_PROFILES, json!({}))
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// 新增/改一套（顺带切成当前），或只带 `id` + `makeActive` 切换。
pub async fn model_save(
    Extension(app): Extension<Arc<AppServer>>,
    Json(params): Json<ModelSaveParams>,
) -> Response {
    let payload = serde_json::to_value(&params).expect("ModelSaveParams 一定能序列化");
    match app
        .request::<ModelSaveResponse>(methods::MODEL_SAVE, payload)
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// 删一套（连它的凭据一起）。
pub async fn model_remove(
    Extension(app): Extension<Arc<AppServer>>,
    Json(params): Json<ModelRemoveParams>,
) -> Response {
    let payload = serde_json::to_value(&params).expect("ModelRemoveParams 一定能序列化");
    match app
        .request::<ModelRemoveResponse>(methods::MODEL_REMOVE, payload)
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

pub async fn approval(
    Extension(app): Extension<Arc<AppServer>>,
    Json(params): Json<ApprovalRespondParams>,
) -> Response {
    let payload = serde_json::to_value(&params).expect("ApprovalRespondParams 一定能序列化");
    // 与 turn 走同一条路：app-server 是子进程，回答也是它的一个方法。
    match app
        .request::<ApprovalRespondResponse>(methods::APPROVAL_RESPOND, payload)
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => failed(StatusCode::OK, error),
    }
}

/// `POST /api/turn` —— 说一句话，看它怎么回。
///
/// 载荷就是协议里的 `TurnStartParams`（**没有第二个形状**），
/// 应答是一条 SSE 流：`data:` 后面跟着 `ServerNotification` 的 JSON，
/// 跟 stdio 上收到的是同一种东西。
///
/// 为什么是 SSE 而不是 WebSocket：这条路上只有「服务端 → 浏览器」这一个
/// 方向，发消息本来就是一次普通的 POST。双向的通道会带来一堆这里用不上的
/// 状态管理，而且浏览器原生的 EventSource 只能 GET，发不了这个 POST。
pub async fn turn(
    Extension(app): Extension<Arc<AppServer>>,
    Json(params): Json<TurnStartParams>,
) -> Response {
    if params.message.trim().is_empty() {
        return failed(
            StatusCode::BAD_REQUEST,
            ErrorObject {
                code: codes::INVALID_PARAMS,
                message: "消息是空的".to_string(),
                data: None,
            },
        );
    }

    let session_id = params
        .session_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "default".to_string());

    let events = app.subscribe();
    let (tx, rx) = mpsc::channel::<ServerNotification>(TURN_BUFFER);

    let payload = serde_json::to_value(&params).expect("TurnStartParams 一定能序列化");
    tokio::spawn(async move {
        forward(&app, payload, session_id, tx, events).await;
    });

    Sse::new(bytes_stream(rx))
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// 把「这一轮」的通知一条条转给浏览器。两条输入：订阅到的通知，和请求的应答。
///
/// 请求的应答（`turn/start` 的返回值）多数时候**没人关心** —— `turnCompleted`
/// 已经把回复、用量、遥测全带回来了。但请求也可能直接失败（模型没配好、
/// 参数不对），那时它带的是唯一的一句话，必须让页面上看见。
async fn forward(
    app: &AppServer,
    payload: serde_json::Value,
    session_id: String,
    tx: mpsc::Sender<ServerNotification>,
    mut events: broadcast::Receiver<ServerNotification>,
) {
    let request = app.request::<TurnStartResponse>(methods::TURN_START, payload);
    tokio::pin!(request);
    let mut answered = false;
    let mut turn_id: Option<String> = None;

    loop {
        tokio::select! {
            result = &mut request, if !answered => {
                answered = true;
                if let Err(error) = result {
                    // 交给页面之后就收工：这一轮根本没跑起来。
                    let _ = tx.send(crate::as_notification(error)).await;
                    break;
                }
            }
            received = events.recv() => {
                let notification = match received {
                    Ok(notification) => notification,
                    // 被追上了：中间的通知没跟上。已经收到的内容不回滚，但要说
                    // 一句，否则页面上是「莫名其妙少了一段」。
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        eprintln!("(joy) 通知推得太快，这一轮丢了 {missed} 条");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };

                let Some(id) = belongs(&notification, &session_id, turn_id.as_deref()) else {
                    // 别人的一轮（网关那边另一个会话跑起来了）。广播是给所有
                    // 订阅者的，过滤只能在这里做。
                    continue;
                };
                turn_id = Some(id);

                let last = matches!(notification, ServerNotification::TurnCompleted(_));
                if tx.send(notification).await.is_err() {
                    // 浏览器把标签页关了：这一轮不用再推了，它自己会跑完。
                    break;
                }
                if last {
                    break;
                }
            }
        }
    }
}

/// 这条通知属于这一轮吗？是的话返回它的 turnId。
///
/// 第一帧 `turnStarted` 只能靠 sessionId 认（那时还不知道 turnId），
/// 之后每一帧都带 turnId —— 认 id 就不会把「另一个会话刚好也在跑」搞混。
/// `consolidationCompleted` 和 `graph*` 没有 turnId，它们只在已经认下
/// 这一轮之后才转发。
fn belongs(
    notification: &ServerNotification,
    session_id: &str,
    known: Option<&str>,
) -> Option<String> {
    use ServerNotification as N;
    match notification {
        N::TurnStarted(started) if started.session_id == session_id => {
            Some(started.turn_id.clone())
        }
        _ if known.is_none() => None,
        other => match turn_id_of(other) {
            Some(id) if Some(id) == known => Some(id.to_string()),
            Some(_) => None,
            None => known.map(str::to_string),
        },
    }
}

/// 通知里带的 turnId。没有这个字段的那几种返回 None（见 `belongs`）。
fn turn_id_of(notification: &ServerNotification) -> Option<&str> {
    use ServerNotification as N;
    match notification {
        N::TurnStarted(n) => Some(&n.turn_id),
        N::TextDelta(n) => Some(&n.turn_id),
        N::GateDecided(n) => Some(&n.turn_id),
        N::Retry(n) => Some(&n.turn_id),
        N::ApprovalRequested(n) => Some(&n.turn_id),
        N::GoalRound(n) => Some(&n.turn_id),
        N::ToolStarted(n) => Some(&n.turn_id),
        N::ToolCompleted(n) => Some(&n.turn_id),
        N::TurnCompleted(n) => Some(&n.turn_id),
        N::ConsolidationCompleted(_)
        | N::GraphStarted(_)
        | N::GraphNodeStarted(_)
        | N::GraphNodeEnded(_)
        | N::GraphEnded(_)
        | N::Error(_) => None,
    }
}

/// 通知通道 → SSE 帧。
///
/// 每一帧就是一条通知的 JSON（`data: {...}`），前端 `JSON.parse` 之后按
/// `type` 字段分流 —— 跟 `ServerNotification` 的判别式完全对上，
/// 前端不需要第二套 schema。
fn bytes_stream(
    rx: mpsc::Receiver<ServerNotification>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    stream::unfold(rx, |mut rx| async move {
        let notification = rx.recv().await?;
        let json = serde_json::to_string(&notification).expect("通知一定能序列化");
        Some((Ok(Event::default().data(json)), rx))
    })
}

/// 出错时返回协议自己的 `ErrorObject`，一个字都不改。
///
/// 状态码不是 200：`fetch` 那边能一眼看出「这一趟没成」，
/// 而 body 里还是那个熟悉的 `{code, message}` —— 页面直接显示 message 就够，
/// app-server 写的那些话本来就是给用户看的。
fn failed(status: StatusCode, error: ErrorObject) -> Response {
    (status, Json(error)).into_response()
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod api_tests;
