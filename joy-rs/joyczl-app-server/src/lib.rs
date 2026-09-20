//! `joyczl-app-server` —— JSON-RPC 服务端，Joy 的大脑所在的进程。
//!
//! 整个三语言架构的支点：**只有这个进程打开 state.db**。
//! CLI、dashboard、聊天网关、Python SDK 都是它的客户端，通过 stdio 上的
//! 换行分隔 JSON-RPC 说话。于是：
//!
//!   * 跨线程共享 SQLite 连接的问题不存在（根本不需要共享）；
//!   * 异步只在服务端这一处（网关那边可以留 TypeScript，用现成生态）；
//!   * 状态单一，任何语言的客户端看到的都是同一份记忆。
//!
//! P2b 接上了流式：模型每吐一段文本，客户端就立刻收到一个 `textDelta`。
//! 所有对外输出（通知和应答）都汇进同一条通道，由唯一的 writer 写 stdout
//! —— 顺序由此得到保证，也不会有并发写坏一行的情况。
//!
//! 没配 key 也能启动：记忆和会话的方法照常工作，只有 `turn/*` 会报
//! PROVIDER_ERROR，并把「去哪儿领 key」一并告诉调用方。

mod dispatch;
mod stdio;
mod trace;
mod turn;

#[cfg(test)]
mod turn_tests;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use joyczl_config::Settings;
use joyczl_loop::Interrupt;
use joyczl_protocol::{JsonRpcMessage, ServerNotification, SettingsPatch, SettingsView};
use joyczl_provider::Resolved;
use joyczl_state::{Calendar, Chat, Episodes, Facts};
use joyczl_tools::ToolRegistry;
use sqlx::sqlite::SqlitePool;
use tokio::sync::mpsc;

pub use dispatch::handle;
pub use stdio::run_stdio;
pub use turn::run_turn;

/// 服务端 → 客户端的一帧。通知与应答走同一条通道 —— 顺序因此有了保证。
pub enum Frame {
    Notification(ServerNotification),
    Response(JsonRpcMessage),
}

/// 往客户端送东西的口子。turn 的执行过程中拿着它，文本增量一来就发出去，
/// 而不是等整轮跑完再一次性发。
#[derive(Clone)]
pub struct EventSink {
    tx: mpsc::UnboundedSender<Frame>,
}

impl EventSink {
    pub(crate) fn new(tx: mpsc::UnboundedSender<Frame>) -> Self {
        Self { tx }
    }

    /// 进程内客户端（终端 REPL）用的出口：自己起一个通道，
    /// 通知与应答从 receiver 里逐帧取。顺序保证与 stdio 一致。
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<Frame>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }

    /// 发送失败只有一种可能：writer 已经退了（进程正在退出）。
    /// 那时再纠结这条通知没有意义，所以静默。
    pub(crate) fn notification(&self, notification: ServerNotification) {
        let _ = self.tx.send(Frame::Notification(notification));
    }

    pub(crate) fn response(&self, response: JsonRpcMessage) {
        let _ = self.tx.send(Frame::Response(response));
    }
}

/// 服务端持有的全部东西。
///
/// `resolved` 是 `Option`：没配 key 也要能启动。记忆和会话不依赖模型，
/// 只有 `turn/*` 需要它 —— 那时才把「缺 key」这件事说清楚。
///
/// `settings` / `resolved` 在 `RwLock` 里：`config/write` 会在运行中改它们，
/// 改完的下一轮 turn 就用新值 —— 不必重启进程。锁只护住读改写的几微秒，
/// 里面没有 await。
///
/// `turns` 是在跑的 turn 的取消令牌表：`turn/interrupt` 按 turn_id 找到它
/// 并拨下开关。turn 结束（无论成败）就摘除。
///
/// `Clone`：每个字段都是把手（Arc / 连接池 / 配置），复制一份不复制状态。
/// 图里的 full_agent 节点是 'static 闭包，只能拿自己拥有的东西 ——
/// 它拿的就是这份克隆。
#[derive(Clone)]
pub struct Server {
    pub(crate) facts: Facts,
    pub(crate) episodes: Episodes,
    pub(crate) chat: Chat,
    pub(crate) calendar: Calendar,
    pub(crate) settings: Arc<RwLock<Settings>>,
    pub(crate) resolved: Arc<RwLock<Option<Resolved>>>,
    pub(crate) tools: Arc<ToolRegistry>,
    pub(crate) turns: Arc<Mutex<HashMap<String, Arc<Interrupt>>>>,
    #[allow(dead_code)]
    pub(crate) pool: SqlitePool,
}

impl Server {
    /// 从 state.db 装配。provider 解析失败不致命 —— 记下原因，等 turn 再报。
    pub async fn boot(pool: SqlitePool, settings: Settings) -> Self {
        let resolved = match joyczl_provider::resolve(&settings) {
            Ok(resolved) => Some(resolved),
            Err(reason) => {
                // 打到 stderr：stdout 是协议通道，不能混入日志。
                eprintln!("(joy) 模型还没配好，turn/* 暂不可用 —— {reason}");
                None
            }
        };
        // 工具表要连 MCP（若有配置），所以是 async 的 —— 装配顺序上只能
        // 先把它建好，再塞进 Server。
        let tools = builtin_tools(&settings).await;
        Self {
            facts: Facts::new(pool.clone()),
            episodes: Episodes::new(pool.clone()),
            chat: Chat::new(pool.clone()),
            calendar: Calendar::new(pool.clone()),
            settings: Arc::new(RwLock::new(settings)),
            resolved: Arc::new(RwLock::new(resolved)),
            tools: Arc::new(tools),
            turns: Arc::new(Mutex::new(HashMap::new())),
            pool,
        }
    }

    /// 当前设置的快照。拿到的副本随便用多久都行 —— 并发写只会影响下一轮。
    pub fn settings(&self) -> Settings {
        self.settings.read().expect("settings 锁不该中毒").clone()
    }

    // ---- 进程内客户端（终端 REPL）的读面 --------------------------------
    // 字段本身是 pub(crate)：写路径只许走协议方法；这几把只读把手是给
    // /memory、/sessions 这类本地命令用的。

    pub fn facts(&self) -> &Facts {
        &self.facts
    }

    pub fn episodes(&self) -> &Episodes {
        &self.episodes
    }

    pub fn chat(&self) -> &Chat {
        &self.chat
    }

    /// 当前 provider 解析结果的快照（没配 key 时是 None）。
    pub fn resolved(&self) -> Option<Resolved> {
        self.resolved.read().expect("resolved 锁不该中毒").clone()
    }

    pub(crate) fn set_settings(&self, settings: Settings) {
        *self.settings.write().expect("settings 锁不该中毒") = settings;
    }

    pub(crate) fn set_resolved(&self, resolved: Option<Resolved>) {
        *self.resolved.write().expect("resolved 锁不该中毒") = resolved;
    }

    /// 注入一个 provider 解析结果。评测与测试的注入口：正常路径走
    /// `joyczl_provider::resolve`（环境变量给 key），eval 需要 scripted
    /// 模型才能离线、确定地钉住 harness 行为 —— 那时从外面塞一个进来。
    pub fn install_provider(&self, resolved: Resolved) {
        self.set_resolved(Some(resolved));
    }

    /// 登记一个正在跑的 turn，发它的取消令牌。turn 结束时必须 `finish_turn`。
    pub(crate) fn register_turn(&self, turn_id: &str) -> Arc<Interrupt> {
        let interrupt = Interrupt::new();
        self.turns
            .lock()
            .expect("turns 锁不该中毒")
            .insert(turn_id.to_string(), interrupt.clone());
        interrupt
    }

    /// 摘掉一个 turn。跑完了就没有可打断的东西。
    pub(crate) fn finish_turn(&self, turn_id: &str) {
        self.turns.lock().expect("turns 锁不该中毒").remove(turn_id);
    }

    /// 打断一个在跑的 turn。返回 false = 没找到（已经跑完了）。
    /// 公开给 eval 用：interrupt 场景要在流式中途拨令牌。
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

    /// `config/write` 的执行体：校验补丁 → 累计落盘 → 套用 → 重解析 provider。
    ///
    /// 校验失败返回带原因的错误，什么都没改 —— 补丁是全有或全无的，
    /// 半个补丁比没有更糟。
    pub(crate) fn apply_config_patch(&self, patch: &SettingsPatch) -> Result<SettingsView, String> {
        validate_patch(patch)?;

        // 累计：已保存的补丁是新补丁的地基。落盘失败就不动内存 ——
        // 「改了但不记得」比「没改成」更撒谎。
        let home = self.settings().home.clone();
        let mut saved = joyczl_config::load_patch(&home);
        saved.merge_newer(patch);
        joyczl_config::save_patch(&home, &saved)
            .map_err(|e| format!("写 settings.json 失败：{e}"))?;

        let mut settings = self.settings();
        joyczl_config::apply_patch(&saved, &mut settings);

        // 换了 provider / model 就重新解析。解析失败不致命 —— 记下原因，
        // turn/* 会把「缺 key」说清楚；旧 client 已被替换，不该再被用。
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

/// config/write 的字段校验。宁可在这里多啰嗦一句，也不让一个非法值
/// 悄悄住进 settings.json，每一轮都发作一次。
fn validate_patch(patch: &SettingsPatch) -> Result<(), String> {
    if let Some(provider) = &patch.provider {
        let provider = provider.trim();
        if joyczl_provider::lookup(provider).is_none() {
            let ids = joyczl_provider::PROVIDERS
                .iter()
                .map(|p| p.id)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!("未知的 provider '{provider}'。可选：{ids}"));
        }
    }
    for (value, name, min) in [
        (patch.max_iterations, "maxIterations", 1),
        (patch.max_tokens, "maxTokens", 128),
        (patch.history_turns, "historyTurns", 0),
        (patch.consolidate_every, "consolidateEvery", 1),
        (patch.retrieval_top_k, "retrievalTopK", 1),
    ] {
        if let Some(v) = value {
            if v < min {
                return Err(format!("{name} 至少是 {min}，收到 {v}"));
            }
        }
    }
    Ok(())
}

/// 打开 `settings.home` 下的 state.db 并装配。
pub async fn open(settings: &Settings) -> Result<Server> {
    // 先把已保存的 config/write 补丁叠上来：环境变量是地基，
    // settings.json 里的显式覆盖说话更晚、声音更大。
    let mut settings = settings.clone();
    let saved = joyczl_config::load_patch(&settings.home);
    joyczl_config::apply_patch(&saved, &mut settings);
    settings.ensure_home()?;
    let pool = joyczl_state::open(&settings.home.join("state.db")).await?;
    Ok(Server::boot(pool, settings).await)
}

/// 本家工具，外加 `<home>/mcp.json` 里配的 MCP 工具。
///
/// 一个 MCP 服务器连不上只是往 stderr 留一句警告，Joy 照常启动 ——
/// 配错的服务器不该让整个助理起不来（跟「工具执行失败不该让一轮对话崩掉」
/// 是同一条规矩）。没有 mcp.json 就等于没配，不读文件、不起进程、不连网。
async fn builtin_tools(settings: &Settings) -> ToolRegistry {
    let mut tools = joyczl_tools::handlers::build_default();
    let mcp = joyczl_mcp::McpClient::connect(&settings.home.join("mcp.json")).await;
    for warning in mcp.warnings() {
        // 跟上面那条一样：stdout 是协议通道，日志只能走 stderr。
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
    tools
}

/// 协议里的整数字段是 i32（理由见 joyczl-protocol 的模块文档），存储层是 i64。
/// 越界时夹到边界而不是 panic —— 记忆库里真有 21 亿条事实的话，
/// 编号错乱也远好过整个 app-server 崩掉。
pub(crate) fn narrow(value: i64) -> i32 {
    value.clamp(0, i32::MAX as i64) as i32
}
