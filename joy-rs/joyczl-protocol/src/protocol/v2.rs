//! Joy app-server v2 —— 契约本体。
//!
//! 这一份定义同时喂给三方：Rust app-server（直接用）、TypeScript 前端
//! （ts-rs 生成）、Python SDK（schemars → datamodel-code-generator）。
//! **改这里就是改协议**，改完必须跑 `just write-app-server-schema`。
//!
//! 命名规则：
//!   * `*Params`        客户端 → 服务端的请求载荷
//!   * `*Response`      服务端 → 客户端的应答载荷
//!   * `*Notification`  服务端 → 客户端的推送
//!   * wire 上一律 camelCase；枚举值也 camelCase
//!   * 时间戳用 Unix 秒（`*_at`）或 ISO 8601 字符串（面向人类展示的字段）
//!   * **整数字段一律 `i32`**，不用 `i64`：`i64` 会被 ts-rs 映射成 TypeScript 的
//!     `bigint`，而 `JSON.parse` 出来的是 `number`，类型对不上会在运行时炸。
//!     这里的每个整数都是计数 / id / 毫秒，量级远小于 2^31；存储层内部仍可用
//!     i64（SQLite rowid），跨过协议边界时再收窄。

use serde::{Deserialize, Serialize};

use crate::{JsonSchema, TS};

/// JSON-RPC 方法名。`<resource>/<method>`，resource 单数。
pub mod methods {
    pub const TURN_START: &str = "turn/start";
    pub const TURN_INTERRUPT: &str = "turn/interrupt";
    pub const APPROVAL_RESPOND: &str = "approval/respond";
    pub const SESSION_LIST: &str = "session/list";
    pub const SESSION_NEW: &str = "session/new";
    pub const SESSION_MESSAGES: &str = "session/messages";
    pub const MEMORY_SEARCH: &str = "memory/search";
    pub const MEMORY_LIST: &str = "memory/list";
    pub const MEMORY_LIST_EPISODES: &str = "memory/list-episodes";
    pub const MEMORY_REMEMBER: &str = "memory/remember";
    pub const MEMORY_FORGET: &str = "memory/forget";
    pub const CONFIG_READ: &str = "config/read";
    pub const CONFIG_WRITE: &str = "config/write";
    pub const MODEL_LIST: &str = "model/list";
}

// ===========================================================================
// 通用
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TokenUsage {
    pub input_tokens: i32,
    pub output_tokens: i32,
}

/// 检索门的判定：这一轮到底要不要翻记忆。
/// Joy 的 hero moment —— 默认全量检索既慢又会让无关记忆带偏答案。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum GateDecisionKind {
    Retrieve,
    Skip,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct GateDecision {
    pub decision: GateDecisionKind,
    /// 门给的理由（≤5 个词的那种）。失败开放时带上原因，便于排查。
    pub reason: String,
    /// 判定为 retrieve 时的检索词；skip 时为 null。
    pub query: Option<String>,
}

/// triage graph 把这一轮判成「快回答」还是「完整走一圈」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum GraphRouteKind {
    Quick,
    Full,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct GraphInfo {
    pub workflow: String,
    pub route: GraphRouteKind,
    pub reason: String,
    /// 实际走过的节点名，顺序即执行顺序。
    pub path: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum ToolStatus {
    Ok,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ToolCallRecord {
    pub tool: String,
    pub status: ToolStatus,
    pub duration_ms: Option<i32>,
}

/// 一次「要不要执行」的询问。
///
/// 服务端把问题抛给客户端后**等待**（默认 120 秒），回答由 `approval/respond`
/// 送来。没人回答、超时、或者根本没有批准通道 —— 一律**按拒绝处理**：默认
/// 拒绝是这个功能的地基，不是它的边界情况。
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ApprovalRequestedNotification {
    pub turn_id: String,
    /// 这一轮内的请求号，回答时要原样带回来。
    pub request_id: String,
    /// 想执行什么（工具名，目前只有 `run_command`）。
    pub tool: String,
    /// 给人看的动作预览：命令原文，或参数摘要。
    pub args_preview: String,
    /// 为什么没被放行规则直接放行。
    pub reason: String,
    /// 多少毫秒内没人回答就按拒绝算。
    pub expires_in_ms: i32,
}

/// `approval/respond` 的参数。
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ApprovalRespondParams {
    pub turn_id: String,
    pub request_id: String,
    pub approved: bool,
    /// 记住这个决定：把这条命令本身写进 `<home>/settings.json` 的放行表
    /// （不加通配 —— 用户批准的是这条命令，不是这一类）。
    #[serde(default)]
    pub remember: bool,
}

/// `approval/respond` 的应答。
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ApprovalRespondResponse {
    /// 这个请求还在等人回答吗？太晚送达（已超时或那一轮已经结束）时为 false ——
    /// 客户端据此知道「我的回答没人听」，而不是以为批准生效了。
    pub accepted: bool,
}

/// 一轮 turn 的遥测。落库到 chat_log.meta，所以重开一个旧会话
/// 仍能看到当时是哪个脑子的哪个决定，而不只是最终那句回复。
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnMeta {
    pub gate: Option<GateDecision>,
    /// graph 没介入时为 null。
    pub graph: Option<GraphInfo>,
    pub iterations: i32,
    pub latency_ms: i32,
    pub tools: Vec<ToolCallRecord>,
    /// 真正回答这一轮的模型：被 graph 判成 quick 时是小模型，如实记录。
    pub model: String,
    pub provider: String,
    /// 这一轮的 token 用量。只走过 `turnCompleted`、没进库的历史缺陷
    /// 已经补上：现在它随 meta 一起落进 chat_log，重看历史也看得到花了多少。
    #[serde(default)]
    pub usage: Option<TokenUsage>,
    /// 这一轮被 `turn/interrupt` 打断时为 true。
    /// 旧库里的 meta 没有这个字段 —— `#[serde(default)]` 让它们照常解析。
    #[serde(default)]
    pub interrupted: bool,
    /// 循环护栏在这一轮命中几次（0 = 模型没有卡在重复/交替的工具调用里）。
    /// 是 0 才正常；不为 0 说明这一轮的预算有一部分花在了原地打转上。
    #[serde(default)]
    pub guard_hits: i32,
    /// 命中时护栏对模型说的那句话（给人看的解释；没命中就是 null）。
    #[serde(default)]
    pub guard_note: Option<String>,
    /// 这一轮里 provider 重试了几次（限流/临时故障）。0 是常态。
    #[serde(default)]
    pub retries: i32,
}

// ===========================================================================
// 记忆
// ===========================================================================

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct Fact {
    pub id: i32,
    pub subject: String,
    pub content: String,
    /// `user`（直接告诉它的）或 `consolidation`（从对话里提炼的）。
    pub source: String,
    /// 这条事实**关于什么**：`user` / `feedback` / `project` / `reference` /
    /// `fact`（兜底）。分类不出来的都落在 `fact`。
    pub kind: String,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct Episode {
    pub id: i32,
    /// ISO 8601 日期。
    pub happened_at: String,
    pub summary: String,
    pub created_at: Option<String>,
}

// ===========================================================================
// 会话 / 模型 / 配置
// ===========================================================================

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionSummary {
    pub id: String,
    /// 会话标题 = 第一条用户消息的前 60 个字。
    pub title: String,
    pub messages: i32,
    pub started_at: Option<String>,
    pub last_at: Option<String>,
}

/// 消息的角色。**闭集**，故意不留「未知」这个口子。
///
/// `chat_log` 里将来若真多出别的角色，`session/messages` 会**跳过**那些行，
/// 而不是硬塞一个值进来 —— 前端那个 `switch` 才能是穷尽的，
/// 「还有第三种角色要画」这种事就会在编译期现形。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum MessageRole {
    User,
    Assistant,
}

/// 会话历史里的一条消息：`chat_log` 的一行。
///
/// 跟实时那一路（`turnStarted` / `textDelta` / `turnCompleted`）说的是同一场
/// 对话的两个视角：实时那一路只为**这一次连接**活着（刷新即消失），
/// 这一条让「刷新、换台设备、换个时候再看」成为可能。
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct Message {
    /// chat_log 的行号。**也是游标**：`session/messages` 的 cursor 就是它。
    /// 用行号而不是时间戳：同一轮的两行是同一秒写进去的，时间戳分不出先后。
    pub id: i32,
    pub role: MessageRole,
    pub content: String,
    /// ISO 8601（UTC）。写进来的时刻。
    pub at: String,
    /// 这一轮的遥测（检索门 / 工具 / 迭代 / 模型 / usage），**只有 assistant 行上有**。
    pub meta: Option<TurnMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ModelInfo {
    pub id: String,
    pub provider: String,
    pub label: Option<String>,
    pub flagship: Option<String>,
    pub fast: Option<String>,
}

/// 配置的**读取视图**。字段刻意全量且非可选：读出来就该是完整现状。
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SettingsView {
    pub provider: String,
    pub model: String,
    pub small_model: String,
    pub max_iterations: i32,
    pub max_tokens: i32,
    /// 工作记忆滑窗：只把最近 N 轮塞进 prompt。
    pub history_turns: i32,
    pub consolidate_every: i32,
    pub retrieval_top_k: i32,
    pub apple_calendar: bool,
    pub google_calendar: bool,
    pub experimental: bool,
    pub graph_workflows: bool,
    pub home: String,
}

/// 配置的**写入补丁**。只传要改的字段，null 表示不改。
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SettingsPatch {
    #[ts(optional = nullable)]
    pub provider: Option<String>,
    #[ts(optional = nullable)]
    pub model: Option<String>,
    #[ts(optional = nullable)]
    pub small_model: Option<String>,
    #[ts(optional = nullable)]
    pub max_iterations: Option<i32>,
    #[ts(optional = nullable)]
    pub max_tokens: Option<i32>,
    #[ts(optional = nullable)]
    pub history_turns: Option<i32>,
    #[ts(optional = nullable)]
    pub consolidate_every: Option<i32>,
    #[ts(optional = nullable)]
    pub retrieval_top_k: Option<i32>,
    #[ts(optional = nullable)]
    pub apple_calendar: Option<bool>,
    #[ts(optional = nullable)]
    pub google_calendar: Option<bool>,
    #[ts(optional = nullable)]
    pub experimental: Option<bool>,
    #[ts(optional = nullable)]
    pub graph_workflows: Option<bool>,
    /// 放行表（`JOY_EXEC_ALLOW`）的整表替换 —— 「记住这条命令」写的就是它。
    #[ts(optional = nullable)]
    pub exec_allow: Option<Vec<String>>,
}

impl SettingsPatch {
    /// 一个要改的字段都没有。
    pub fn is_empty(&self) -> bool {
        let p = self;
        p.provider.is_none()
            && p.model.is_none()
            && p.small_model.is_none()
            && p.max_iterations.is_none()
            && p.max_tokens.is_none()
            && p.history_turns.is_none()
            && p.consolidate_every.is_none()
            && p.retrieval_top_k.is_none()
            && p.apple_calendar.is_none()
            && p.google_calendar.is_none()
            && p.experimental.is_none()
            && p.graph_workflows.is_none()
    }

    /// 用 `newer` 里非空的字段盖掉自己 —— 累计补丁时用：
    /// 已保存的补丁是新补丁的地基，而不是反过来。
    pub fn merge_newer(&mut self, newer: &Self) {
        let p = newer;
        if p.provider.is_some() {
            self.provider = p.provider.clone();
        }
        if p.model.is_some() {
            self.model = p.model.clone();
        }
        if p.small_model.is_some() {
            self.small_model = p.small_model.clone();
        }
        if p.max_iterations.is_some() {
            self.max_iterations = p.max_iterations;
        }
        if p.max_tokens.is_some() {
            self.max_tokens = p.max_tokens;
        }
        if p.history_turns.is_some() {
            self.history_turns = p.history_turns;
        }
        if p.consolidate_every.is_some() {
            self.consolidate_every = p.consolidate_every;
        }
        if p.retrieval_top_k.is_some() {
            self.retrieval_top_k = p.retrieval_top_k;
        }
        if p.apple_calendar.is_some() {
            self.apple_calendar = p.apple_calendar;
        }
        if p.google_calendar.is_some() {
            self.google_calendar = p.google_calendar;
        }
        if p.experimental.is_some() {
            self.experimental = p.experimental;
        }
        if p.graph_workflows.is_some() {
            self.graph_workflows = p.graph_workflows;
        }
        if p.exec_allow.is_some() {
            self.exec_allow = p.exec_allow.clone();
        }
    }
}

// ===========================================================================
// 请求 / 应答
// ===========================================================================

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnStartParams {
    #[ts(optional = nullable)]
    pub session_id: Option<String>,
    pub message: String,
    /// 是否逐 token 推送 `textDelta`。dashboard 开着，脚本调用关着。
    #[ts(optional = nullable)]
    pub stream: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnStartResponse {
    pub turn_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnInterruptParams {
    pub turn_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnInterruptResponse {
    /// false = 这一轮已经跑完了，没打断到什么。
    pub interrupted: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionListParams {
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionListResponse {
    pub data: Vec<SessionSummary>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionNewParams {
    /// 不给就由服务端生成。
    #[ts(optional = nullable)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionNewResponse {
    pub session_id: String,
}

/// 读一个会话说过的话。
///
/// **为什么需要它**：没有这个方法，页面上那场对话就只活在浏览器里 ——
/// 刷新一下全没了。它同时是「切换会话」的前提：切过去至少能看见那边说过什么。
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionMessagesParams {
    pub session_id: String,
    /// 只取比这条消息**更早**的（往上翻旧账）。不给就是最新的那一页。
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

/// 消息**最新的在最前** —— 跟对话本身的方向相反，因为「往上翻」才是
/// 这个方法的用法：先拿最近一页，再顺着 `nextCursor` 往更早的地方走。
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionMessagesResponse {
    pub data: Vec<Message>,
    /// 还有更早的才给：把它当下一次的 cursor。null = 到底了。
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MemorySearchParams {
    pub query: String,
    #[ts(optional = nullable)]
    pub top_k: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MemorySearchResponse {
    pub facts: Vec<Fact>,
    pub episodes: Vec<Episode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MemoryRememberParams {
    pub subject: String,
    pub content: String,
    /// 不给就是 `user`（用户直接说的）。consolidation 从对话里提炼时传
    /// `consolidation`，这样前端能区分「你告诉它的」和「它自己总结的」。
    #[ts(optional = nullable)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MemoryRememberResponse {
    /// 写进去的完整 fact，带 id —— 前端可以直接插进列表，不用再查一次。
    pub fact: Fact,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MemoryForgetParams {
    pub subject: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MemoryForgetResponse {
    /// 实际删掉多少条。0 表示本来就没有这个主题的记忆。
    pub removed: i32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MemoryListParams {
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MemoryListResponse {
    pub data: Vec<Fact>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MemoryListEpisodesParams {
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

/// 情景记忆的列表。**没有 cursor**：`Episodes::recent` 只按时间倒序取最近 N 条，
/// 存储层没有偏移量的概念。与其编一个用不了的游标，不如诚实地返回 null。
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MemoryListEpisodesResponse {
    pub data: Vec<Episode>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ConfigReadParams {
    /// 只取这几个 key；不给就全量返回。
    #[ts(optional = nullable)]
    pub keys: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ConfigReadResponse {
    pub config: SettingsView,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ConfigWriteParams {
    pub patch: SettingsPatch,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ConfigWriteResponse {
    /// 写入后的完整配置，便于前端直接替换本地缓存。
    pub config: SettingsView,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ModelListParams {
    #[ts(optional = nullable)]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ModelListResponse {
    pub data: Vec<ModelInfo>,
    pub next_cursor: Option<String>,
}

// ===========================================================================
// 驾驶舱
// ===========================================================================

/// 驾驶舱首屏要的全部东西，一次取齐。
///
/// 它不是某个 RPC 的应答，而是 `joy dashboard` 那个 HTTP 端点
/// （`GET /api/data`）的载荷。**为什么定义在协议 crate 里**：因为判断标准
/// 只有一条 —— 它跨语言。浏览器那一侧要按它做类型检查，手抄一遍就又会
/// 漂移，而这正是整个工程在消灭的东西。
///
/// 字段都直接复用协议里的类型：驾驶舱不做二次建模，它就是这些数据的
/// 一个视图。
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct DashboardData {
    /// ISO 8601，毫秒精度。页面上显示「数据是什么时候的」。
    pub generated_at: String,
    pub config: SettingsView,
    pub sessions: Vec<SessionSummary>,
    pub facts: Vec<Fact>,
    pub episodes: Vec<Episode>,
}

// ===========================================================================
// 通知（服务端 → 客户端，一轮 turn 的全部过程）
// ===========================================================================

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnStartedNotification {
    pub turn_id: String,
    pub session_id: String,
    pub user_message: String,
    /// ISO 8601，毫秒精度。
    pub ts: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TextDeltaNotification {
    pub turn_id: String,
    /// 流式文本增量。只在 `turn/start` 传了 `stream: true` 时才有。
    pub delta: String,
}

/// 一次「正在重试」的说明。
///
/// 重试**从不静默**：用户看到的是这句话，而不是一段莫名其妙的长时间停顿。
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct RetryNotification {
    pub turn_id: String,
    /// 第几次重试（从 1 开始）。
    pub attempt: i32,
    /// 为什么重试（`HTTP 429` / `网络错误：…`）。
    pub reason: String,
    /// 等了多久。
    pub delay_ms: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct GateDecidedNotification {
    pub turn_id: String,
    pub decision: GateDecision,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ToolStartedNotification {
    pub turn_id: String,
    pub tool: String,
    /// 模型给的参数，结构随工具而定。
    #[ts(type = "unknown")]
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ToolCompletedNotification {
    pub turn_id: String,
    pub tool: String,
    /// 工具回给模型看的文本。失败的详细信息也在这里 —— 不中断 turn。
    pub output: String,
    pub status: ToolStatus,
    pub duration_ms: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ConsolidationCompletedNotification {
    /// 这一批提炼出多少条新 fact；0 表示没到期或没提炼出东西。
    pub new_facts: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct GraphStartedNotification {
    pub workflow: String,
    pub nodes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct GraphNodeStartedNotification {
    pub workflow: String,
    pub node: String,
    /// 第几次进入该节点（>1 只可能出现在有意设计的环里）。
    pub visit: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct GraphNodeEndedNotification {
    pub workflow: String,
    pub node: String,
    pub ms: i32,
    /// 该节点写回了哪些 state key。
    pub keys: Vec<String>,
    /// 节点抛错时是错误文本；错误只记录、不外抛，整个 run 会正常收尾。
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct GraphEndedNotification {
    pub workflow: String,
    pub ms: i32,
    pub steps: i32,
    pub path: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnCompletedNotification {
    pub turn_id: String,
    pub reply: String,
    pub iterations: i32,
    pub meta: TurnMeta,
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ErrorNotification {
    pub code: i32,
    pub message: String,
    #[ts(type = "unknown")]
    pub data: Option<serde_json::Value>,
}

/// 一轮 turn 的全部过程事件。
///
/// `type` 是判别式 —— 前端拿它做 switch，Python 侧用 pydantic discriminated
/// union。**任何失败都不应让客户端崩**：这里只描述「发生了什么」。
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(tag = "type")]
#[ts(export_to = "v2/")]
pub enum ServerNotification {
    TurnStarted(TurnStartedNotification),
    TextDelta(TextDeltaNotification),
    GateDecided(GateDecidedNotification),
    /// 正在重试（限流/临时故障）：**每次必发**，不静默。
    Retry(RetryNotification),
    /// 有一条命令在等人批准：先回它，那一轮才会继续。
    ApprovalRequested(ApprovalRequestedNotification),
    ToolStarted(ToolStartedNotification),
    ToolCompleted(ToolCompletedNotification),
    ConsolidationCompleted(ConsolidationCompletedNotification),
    GraphStarted(GraphStartedNotification),
    GraphNodeStarted(GraphNodeStartedNotification),
    GraphNodeEnded(GraphNodeEndedNotification),
    GraphEnded(GraphEndedNotification),
    /// 装了 `Box`：它带的 `TurnMeta` 比别的变体大一个量级，而通知是按事件
    /// 构造、在通道里搬运的。ts-rs 对 `Box<T>` 透明，TS/Python 生成物不受影响。
    TurnCompleted(Box<TurnCompletedNotification>),
    Error(ErrorNotification),
}
