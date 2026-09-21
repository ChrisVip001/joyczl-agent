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

mod approval;
mod dispatch;
mod stdio;
mod subagent;
mod trace;
mod turn;

#[cfg(test)]
mod turn_tests;

#[cfg(test)]
#[path = "approval_tests.rs"]
mod approval_tests;

use std::collections::HashMap;
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
    /// 在等人回答的批准请求：`approval/respond` 按 turn_id + request_id 找到
    /// 它，把回答送回去。与 `turns` 同一条生命周期纪律（一轮结束整桶摘掉）。
    pub(crate) approvals: approval::Pending,
    #[allow(dead_code)]
    pub(crate) pool: SqlitePool,
}

impl Server {
    /// 从 state.db 装配。provider 解析失败不致命 —— 记下原因，等 turn 再报。
    pub async fn boot(pool: SqlitePool, settings: Settings) -> Self {
        let settings = Arc::new(RwLock::new(settings));
        // 读锁只在表达式里拿一下：**绝不跨 await 持有**（clippy 会拦，也确实
        // 该拦 —— 一个被 await 卡住的读锁会把整台服务器堵死）。
        let snapshot = settings.read().expect("settings 锁不该中毒").clone();
        let resolved = match joyczl_provider::resolve(&snapshot) {
            Ok(resolved) => Some(resolved),
            Err(reason) => {
                // 打到 stderr：stdout 是协议通道，不能混入日志。
                eprintln!("(joy) 模型还没配好，turn/* 暂不可用 —— {reason}");
                None
            }
        };
        let resolved = Arc::new(RwLock::new(resolved));
        // 工具表要连 MCP（若有配置），所以是 async 的 —— 装配顺序上只能
        // 先把它建好，再塞进 Server。
        // 技能的依赖缺失在**启动时**说一次（而不是每轮都说）：它影响的是
        // 「这个技能还能不能隐式触发」，那是个装载期的事实。
        {
            let mut loader = joyczl_memory::skills::SkillLoader::new(
                joyczl_memory::skills::SkillLoader::dirs_for(&snapshot.home),
            );
            for (skill, missing) in loader.dangling_dependencies() {
                eprintln!("(joy) 技能 '{skill}' 依赖的 '{missing}' 不在，它不参与隐式触发");
            }
        }

        let mut tools = builtin_tools(&snapshot).await;
        let facts = Facts::new(pool.clone());
        let episodes = Episodes::new(pool.clone());
        let chat = Chat::new(pool.clone());
        let calendar = Calendar::new(pool.clone());

        // 子代理：**默认关着**，开了才注册。runner 拿到的是「这一份工具表的
        // 副本」——**注意 `tools.clone()` 在 register 之前求值**，所以副本里
        // 没有 `delegate_task`：递归派生于是不是「被拒绝」，而是根本不存在
        // 这个选项。顺序反过来就等于把递归打开。
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
                    // 共享句柄，不是快照：换 provider 之后进来的这一轮要用新的。
                    settings: settings.clone(),
                    resolved: resolved.clone(),
                },
            )));
        }

        Self {
            facts,
            episodes,
            chat,
            calendar,
            settings,
            resolved,
            tools: Arc::new(tools),
            turns: Arc::new(Mutex::new(HashMap::new())),
            approvals: Arc::new(Mutex::new(HashMap::new())),
            pool,
        }
    }

    /// 执行工具时给 handler 的环境。**只有这一处构造**（turn 与子代理共用）——
    /// 加字段时不会漏掉某条路径。
    ///
    /// `approval` 是这一轮的批准通道：子代理传 `None`（它不该阻塞在人类身上）。
    pub(crate) fn tool_ctx(
        &self,
        approval: Option<Arc<dyn joyczl_tools::approval::ApprovalBroker>>,
    ) -> ToolCtx {
        let settings = self.settings();
        tool_ctx(
            &self.facts,
            &self.episodes,
            &self.chat,
            &self.calendar,
            &settings.home,
            approval,
        )
    }

    /// 这一轮的批准通道。`never` 模式（默认）返回 `None` —— 没人问，也就没人答，
    /// 需要批准的动作直接拒绝。
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
        }))
    }

    /// 回答一次批准请求。返回 `false` = 太晚了（已超时，或那一轮已经结束）——
    /// 如实告诉客户端，免得它以为批准生效了。
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
        // 送不到（接收端已经走了）不算失败：那一轮已经结束了。
        let _ = waiting.tx.send(approved);
        true
    }

    /// 「记住这条命令」：整表替换放行表，落进 `settings.json`。
    ///
    /// 记的是**这条命令本身**，不加通配 —— 用户看到并批准的是它，不是这一类。
    /// 执行策略在启动时读一次，所以这条要**下次启动**才生效；这一点如实说出来，
    /// 而不是让人以为下次不会再问了。
    fn remember_command(&self, command: &str) {
        let mut settings = self.settings();
        if settings.exec_allow.iter().any(|rule| rule == command) {
            return; // 已经在表里了（大概是同一轮里问了两次）
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
        // 同一个道理：这一轮没回答的批准请求也一并作废（它的接收端已经走了，
        // 留着只会占地方）。
        self.approvals
            .lock()
            .expect("approvals 锁不该中毒")
            .remove(turn_id);
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

/// config/write 的字段校验：数值边界走 `joyczl-config` 的 `BOUNDS`（与启动期
/// 同一张表），这里只补「provider 必须存在」这一条 —— `joyczl-config` 不认识
/// `PROVIDERS`，那是 provider 层的事。
fn validate_patch(patch: &SettingsPatch) -> Result<(), String> {
    joyczl_config::validate_patch_values(patch)?;
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
    Ok(())
}

/// 打开 `settings.home` 下的 state.db 并装配。
pub async fn open(settings: &Settings) -> Result<Server> {
    // 先把已保存的 config/write 补丁叠上来：环境变量是地基，
    // settings.json 里的显式覆盖说话更晚、声音更大。
    let mut settings = settings.clone();
    let saved = joyczl_config::load_patch(&settings.home);
    joyczl_config::apply_patch(&saved, &mut settings);

    // ---- 启动期校验：非法配置当场退出。
    //
    // 这就是「把错误配置变成启动期错误，而不是运行期惊喜」那条纪律的落点。
    // 检查的是**叠加之后**的最终值：环境变量与 settings.json 谁配错了都能
    // 在这一处报出来，而且报的是「哪个变量/字段错了」。
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

/// 本家工具，外加 `<home>/mcp.json` 里配的 MCP 工具。
///
/// 一个 MCP 服务器连不上只是往 stderr 留一句警告，Joy 照常启动 ——
/// 配错的服务器不该让整个助理起不来（跟「工具执行失败不该让一轮对话崩掉」
/// 是同一条规矩）。没有 mcp.json 就等于没配，不读文件、不起进程、不连网。
async fn builtin_tools(settings: &Settings) -> ToolRegistry {
    let mut tools = joyczl_tools::handlers::build_default();
    // 执行工具：**默认关着**，开了才注册 —— 没开的时候模型连它的名字都
    // 看不见。权限最高的一件事，开关必须是用户亲手按下的。
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
        tools.register(joyczl_tools::exec::run_command(
            joyczl_tools::exec::ExecPolicy {
                allow: settings.exec_allow.clone(),
                timeout_secs: settings.exec_timeout_secs,
                network: settings.exec_network,
                // 批准只影响放行表那一关：没匹配上时是拒绝，还是问一句。
                approval: settings.approval == "on-request",
                approval_timeout_secs: settings.approval_timeout_secs,
                // 超长输出落盘：截断仍然发生（上下文要保住），但原文还在。
                spill_dir: Some(settings.home.join("spill")),
                extra_roots: settings.exec_writable_roots.clone(),
            },
        ));
    }
    // 启动时顺手打扫 spill/：只保留最近 7 天（见 docs/limitations.md 里
    // 「不做配额轮转」那条）。与 exec 开关无关 —— 文件在那儿就该打扫。
    joyczl_tools::exec::prune_spill(&settings.home, 7);

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

/// 工具执行环境的**唯一**构造处。turn 与子代理都走它 —— 两处各写一遍
/// 就是等着某天加字段时漏掉一个（子代理拿到半个 ctx 会很难查）。
pub(crate) fn tool_ctx(
    facts: &Facts,
    episodes: &Episodes,
    chat: &Chat,
    calendar: &Calendar,
    home: &std::path::Path,
    approval: Option<Arc<dyn joyczl_tools::approval::ApprovalBroker>>,
) -> ToolCtx {
    ToolCtx {
        facts: facts.clone(),
        episodes: episodes.clone(),
        chat: chat.clone(),
        calendar: calendar.clone(),
        home: home.to_path_buf(),
        approval,
    }
}

/// 协议里的整数字段是 i32（理由见 joyczl-protocol 的模块文档），存储层是 i64。
/// 越界时夹到边界而不是 panic —— 记忆库里真有 21 亿条事实的话，
/// 编号错乱也远好过整个 app-server 崩掉。
pub(crate) fn narrow(value: i64) -> i32 {
    value.clamp(0, i32::MAX as i64) as i32
}
