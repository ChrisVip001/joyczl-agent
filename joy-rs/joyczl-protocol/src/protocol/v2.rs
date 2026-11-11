//! Joy app-server v2 — the contract itself.
//!
//! This single definition feeds three consumers: the Rust app-server (as-is),
//! the TypeScript frontend (ts-rs), the Python SDK (schemars → datamodel-code-generator).
//! **Editing this file means editing the protocol**; run `just write-app-server-schema` after.
//!
//! Naming rules:
//!   * `*Params`        request payload, client → server
//!   * `*Response`      response payload, server → client
//!   * `*Notification`  push message, server → client
//!   * camelCase everywhere on the wire; enum values too
//!   * timestamps: Unix seconds (`*_at`) or ISO 8601 strings (human-facing fields)
//!   * **integer fields are always `i32`**, never `i64`: ts-rs maps `i64` to
//!     TypeScript `bigint` while `JSON.parse` yields `number` — a mismatch
//!     that blows up at runtime. Every integer here is a count / id /
//!     milliseconds, well below 2^31; storage may still use i64, narrowed at the boundary.

use serde::{Deserialize, Serialize};

use crate::{JsonSchema, TS};

/// JSON-RPC method names. `<resource>/<method>`, resource in singular.
pub mod methods {
    pub const TURN_START: &str = "turn/start";
    pub const TURN_INTERRUPT: &str = "turn/interrupt";
    pub const APPROVAL_RESPOND: &str = "approval/respond";
    /// Read a session's todo sheet (the dashboard's task card). Read-only, human-facing.
    pub const TODO_VIEW: &str = "todo/view";
    /// Long tasks: create / list / view / get / runs / artifacts / suspend / resume / cancel.
    /// **All human-only** — the model has no path to any of them.
    pub const TASK_CREATE: &str = "task/create";
    pub const TASK_LIST: &str = "task/list";
    pub const TASK_VIEW: &str = "task/view";
    pub const TASK_GET: &str = "task/get";
    /// Every attempt of one task (how many times it ran, what each attempt cost).
    pub const TASK_RUNS: &str = "task/runs";
    /// The files this task produced.
    pub const TASK_ARTIFACTS: &str = "task/artifacts";
    pub const TASK_SUSPEND: &str = "task/suspend";
    pub const TASK_RESUME: &str = "task/resume";
    pub const TASK_CANCEL: &str = "task/cancel";
    /// Inbound connector mail (0.13.0): the panel's list, and the human's "handle these".
    pub const INBOX_LIST: &str = "inbox/list";
    pub const INBOX_SUMMARIZE: &str = "inbox/summarize";
    pub const SESSION_LIST: &str = "session/list";
    pub const SESSION_NEW: &str = "session/new";
    pub const SESSION_MESSAGES: &str = "session/messages";
    /// One session's own record: name, working directory, message count.
    pub const SESSION_GET: &str = "session/get";
    /// Rename a session or rebind its working directory (`""` clears). **Human-only** —
    /// the model has no path here, exactly like `task/*`.
    pub const SESSION_UPDATE: &str = "session/update";
    /// Which turns are running right now, optionally narrowed to one session. The dashboard's
    /// "正在跑第几轮" and the CLI's "what is happening" both read from here.
    pub const TURN_LIST: &str = "turn/list";
    pub const TURN_STEER: &str = "turn/steer";
    pub const TURN_FOLLOW_UP: &str = "turn/follow-up";
    pub const MEMORY_SEARCH: &str = "memory/search";
    pub const MEMORY_LIST: &str = "memory/list";
    pub const MEMORY_LIST_EPISODES: &str = "memory/list-episodes";
    pub const MEMORY_REMEMBER: &str = "memory/remember";
    pub const MEMORY_FORGET: &str = "memory/forget";
    pub const CONFIG_READ: &str = "config/read";
    pub const CONFIG_WRITE: &str = "config/write";
    pub const MODEL_LIST: &str = "model/list";
    /// List configured model profiles + the active one. **Human-only to change** — the model cannot.
    pub const MODEL_PROFILES: &str = "model/profiles";
    /// Add/update a profile (switching along the way when `make_active` is set). Passing
    /// only `id` + `make_active` means "switch" — switching must not force re-sending a key.
    pub const MODEL_SAVE: &str = "model/save";
    pub const MODEL_REMOVE: &str = "model/remove";
    /// The routing table (`<home>/model-routes.json`): fixed / auto, and tier → profile id.
    /// **Human-only to change** — the model cannot route itself, exactly like `/model`.
    pub const MODEL_ROUTES: &str = "model/routes";
    pub const MODEL_ROUTES_SAVE: &str = "model/routes/save";
    /// Usage summary (migration 0010): per-model aggregates + totals. **Bookkeeping only, no enforcement** —
    /// no quotas, no rate limits; in BYOK mode the cost lands on the user's own vendor bill.
    pub const USAGE_SUMMARY: &str = "usage/summary";
    /// Approval audit (migration 0011): what the model asked and how it went
    /// (approved/denied/timeout/voided). Lets a hub admin later answer "who approved rm -rf".
    pub const APPROVAL_LOG: &str = "approval/log";
    /// Schedule state snapshot (migration 0012): per-job last_fired/claimed/catch_up/delivery.
    /// Zero-arg method. Feeds the dashboard's schedule panel — the second consumer beyond the CLI.
    pub const SCHEDULE_LIST: &str = "schedule/list";
}

// ===========================================================================
// Common
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TokenUsage {
    pub input_tokens: i32,
    pub output_tokens: i32,
}

/// The retrieval gate's verdict: whether this turn should search memory at all.
/// Joy's hero moment — retrieving everything by default is slow and lets irrelevant memories skew answers.
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
    /// The gate's reason (the ≤5-word kind). Present on fail-open so it can be debugged.
    pub reason: String,
    /// The search query when the verdict is retrieve; null when skipping.
    pub query: Option<String>,
}

/// Whether the triage graph routed this turn as a "quick answer" or a "full lap".
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
    /// The node names actually traversed, in execution order.
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

/// A "may I execute this?" query.
///
/// After throwing the question to the client the server **waits** (120 s by default);
/// the answer arrives via `approval/respond`. No reply, timeout, or no approval
/// channel at all — everything is **treated as denied**: default-deny is this feature's foundation, not its edge case.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ApprovalRequestedNotification {
    pub turn_id: String,
    /// Request id within this turn; echo it back verbatim when answering.
    pub request_id: String,
    /// What wants to run (tool name; currently only `run_command`).
    pub tool: String,
    /// Human-facing action preview: the raw command or an args summary.
    pub args_preview: String,
    /// Why the allow rules did not let it through automatically.
    pub reason: String,
    /// How many milliseconds of silence count as a denial.
    pub expires_in_ms: i32,
}

/// Parameters for `approval/respond`.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ApprovalRespondParams {
    pub turn_id: String,
    pub request_id: String,
    pub approved: bool,
    /// Remember this decision: write the command itself into the allowlist in
    /// `<home>/settings.json` (no wildcards — the user approved this command, not this kind).
    #[serde(default)]
    pub remember: bool,
}

// ===========================================================================
// Long tasks (agent_tasks)
// ===========================================================================
//
// A task is what a goal becomes once it has to survive a restart: persisted, addressable,
// and — crucially — **suspendable**. Since 0.14 it also stands on its own: a task may belong
// to no session, may be executed several times, and its "done" is decided by measurable
// acceptance conditions rather than by the model's own opinion.
// The model has no task/* path (human-only, like goal).

/// One acceptance condition — what must be measurably true for the task to count as done.
///
/// The same shape is both the **input** (`task/create`) and the **view** (what is required),
/// so the card can never show a condition the verifier does not check.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct AcceptanceView {
    /// `fileExists` / `fileContains` / `toolsOk`.
    pub kind: String,
    /// Path, relative to the task's working directory (an absolute path must land inside it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Plain substring for `fileContains` — **not** a regex: a badly written pattern must not
    /// be able to stall a whole round.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// Occurrences required (`fileContains`) / successful calls required (`toolsOk`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<i32>,
    /// Tool name for `toolsOk`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// The human's own wording, when they gave one. Absent = derived from the fields above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// One condition's latest measured verdict. Same order as `AgentTaskView.acceptance`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct CheckView {
    /// The condition in the human's words ("report.md 里至少有 3 处结论").
    pub label: String,
    pub passed: bool,
    /// Passed: the evidence ("report.md 12.4 KB"). Failed: the measurement ("report.md 不存在").
    pub detail: String,
}

/// A task's budget. An absent field means that dimension is uncapped.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct BudgetView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_ms: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<i32>,
}

/// What the task has consumed so far. Always present (zeroes when nothing ran yet).
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct SpentView {
    pub wall_ms: i32,
    pub tokens: i32,
    pub tools: i32,
}

/// One task as the human sees it. `status` is a string on the wire
/// (pending / running / suspended / needsAttention / done / failed / cancelled) — the dashboard paints it.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct AgentTaskView {
    pub id: String,
    /// Absent = a task that belongs to no conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub title: String,
    /// The goal loop's condition. Absent = a single-turn task: done when the turn is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_condition: Option<String>,
    pub status: String,
    pub rounds_used: i32,
    pub max_rounds: i32,
    /// The declared model tier (`fast` / `chat` / `reasoning`); absent = per-turn decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// The turn currently carrying this task (a heartbeat; absent when idle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspend_reason: Option<String>,
    /// Why it stopped last time — the card's footnote, never guessed from the status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_stop_reason: Option<String>,
    /// What must be measurably true. Empty = no objective conditions (single-turn task).
    #[serde(default)]
    pub acceptance: Vec<AcceptanceView>,
    /// The latest verdict per condition, same order as `acceptance`.
    #[serde(default)]
    pub checks: Vec<CheckView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetView>,
    pub spent: SpentView,
    /// The task's own working directory (the boundary for verification and writes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// How many times this task has been executed ("执行 3 次").
    #[serde(default)]
    pub run_count: i32,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
}

/// `task/create`: **only a human can start a long task** — there is no tool for it.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct TaskCreateParams {
    /// Absent = the task belongs to no session: it can be created and inspected on its own.
    #[serde(default)]
    pub session_id: Option<String>,
    /// What this task is, in the human's words — the card's first line.
    pub title: String,
    /// The goal condition that keeps the loop going. Absent = single-turn task.
    #[serde(default)]
    pub goal_condition: Option<String>,
    /// Per-task round cap; absent = `JOY_GOAL_MAX_ROUNDS`.
    #[serde(default)]
    pub max_rounds: Option<i32>,
    /// The model tier this task runs on (`fast` / `chat` / `reasoning`). Absent = decide per
    /// turn (auto mode) or use the active profile (fixed mode). A declared tier always wins —
    /// "the task picks the model" is the whole point of the tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// How "done" is measured. Empty = the goal/judge decides, as before.
    #[serde(default)]
    pub acceptance: Vec<AcceptanceView>,
    /// Caps on the whole task's spend. Absent dimensions are uncapped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetView>,
    /// The working directory this task owns. Absent = the turn's own cwd decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct TaskEnvelope {
    pub task: AgentTaskView,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[ts(export_to = "v2/")]
pub struct TaskListParams;

/// Newest first; `active` is the session's newest non-terminal task, when there is one.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct TaskListResponse {
    pub tasks: Vec<AgentTaskView>,
}

/// `task/view`: one session's current task (the card on screen).
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct TaskViewResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<AgentTaskView>,
}

/// `task/get`: address one task by id (a session-less task has no other way in).
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct TaskGetParams {
    pub task_id: String,
}

/// One execution of a task. A task may run several times; each attempt is its own account.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct RunView {
    pub id: String,
    /// running / waiting / suspended / completed / failed / cancelled.
    pub status: String,
    pub rounds: i32,
    pub tokens_in: i32,
    pub tokens_out: i32,
    pub tools_ok: i32,
    pub tools_failed: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
}

/// `task/runs`: every attempt of one task, newest first.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct TaskRunsResponse {
    pub runs: Vec<RunView>,
}

/// One delivered file.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ArtifactView {
    pub path: String,
    pub bytes: i32,
    pub at: String,
}

/// `task/artifacts`: what this task actually produced, newest first.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct TaskArtifactsResponse {
    pub artifacts: Vec<ArtifactView>,
}

/// `task/suspend|resume|cancel`: address one task and say why (the reason is shown, not dropped).
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct TaskActionParams {
    pub task_id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// A task changed state. The event bus folds it into a ping; the hub's `/api/tasks` shows the row.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct TaskUpdatedNotification {
    pub task_id: String,
    /// Absent for a task that belongs to no session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub status: String,
    pub rounds_used: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One received message, as the panel shows it. `snippet` is a first-line preview;
/// the full body stays in state.db — the dashboard never needs to ship it around.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct InboxItemView {
    pub id: i64,
    pub from_addr: String,
    pub subject: String,
    pub received_at: String,
    pub snippet: String,
    pub seen: bool,
}

/// `inbox/list`: what came in, and how much still waits for a person.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct InboxListView {
    pub items: Vec<InboxItemView>,
    pub unseen: i64,
}

/// `inbox/summarize`: the human's "handle these" — turns the unseen mail into one turn.
/// Empty `session_id` means the dedicated `inbox` session.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct InboxSummarizeParams {
    #[serde(default)]
    pub session_id: Option<String>,
}

/// `inbox/summarize`'s answer: how many mails went into the turn (0 = nothing waiting).
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct InboxSummarizeResponse {
    pub count: i64,
    pub session_id: String,
    pub message: String,
}

/// New mail landed in the connector inbox. One knock, no content: the panel pulls the list.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct InboxReceivedNotification {
    /// `email` today.
    pub connector: String,
    /// How many messages this poll added (deduped by uid).
    pub added: i32,
}

/// Which session to look at — the shared params of the dashboard's per-session reads
/// (`todo/view`, `session/get`). One field, one shape, no per-method re-modeling.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionViewParams {
    pub session_id: String,
}

/// One notification per goal-loop verdict — "why did it run another round" is never guessed.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct GoalRoundNotification {
    pub turn_id: String,
    /// Which round (starting at 1).
    pub round: i32,
    pub max_rounds: i32,
    /// `continuing` / `satisfied` / `impossible` / `round-limit` / `blocked`.
    pub status: String,
    /// The judge's reason (or the reason for exiting).
    pub reason: String,
}

/// A checkpoint has been written: this turn's working memory (model thoughts, tool results) is now
/// in state.db — the loop can continue from here after a crash, finished tools are never re-run.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct TurnCheckpointedNotification {
    pub turn_id: String,
    pub session_id: String,
    /// How many loop iterations are already done (a checkpoint lands at each tool-results boundary).
    pub iterations: i32,
}

/// A checkpoint is being resumed: startup reconciliation found it unfinished and the loop
/// continues from where it stood (finished tools are not re-run). The human said earlier
/// "keep going until X" by starting the turn — resuming is not the model assigning itself work.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct TurnResumedNotification {
    pub turn_id: String,
    pub session_id: String,
    /// How many iterations the checkpoint had already completed.
    pub from_iterations: i32,
    /// Why it is resuming (`reconcile: crash recovery`, …).
    pub reason: String,
}

/// A checkpoint is abandoned: the human interrupted the turn (or the session moved on) —
/// it will NOT be auto-resumed. Suspended, not silently lost: the ledger says so.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct TurnSuspendedNotification {
    pub turn_id: String,
    pub session_id: String,
    /// Why it was abandoned (`interrupted by user`, …).
    pub reason: String,
}

/// Response to `approval/respond`.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ApprovalRespondResponse {
    /// Is this request still awaiting an answer? False when the reply arrives too
    /// late (timed out, or the turn already ended) — the client learns nobody heard its answer.
    pub accepted: bool,
}

/// Telemetry for one turn. Persisted into chat_log.meta, so reopening an old
/// session still shows which brain made which decision, not just the final reply.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnMeta {
    pub gate: Option<GateDecision>,
    /// Null when no graph was involved.
    pub graph: Option<GraphInfo>,
    pub iterations: i32,
    pub latency_ms: i32,
    pub tools: Vec<ToolCallRecord>,
    /// The model that actually answered this turn: the small model when the graph routed quick, recorded as-is.
    pub model: String,
    pub provider: String,
    /// Token usage for this turn. The historical flaw of only reaching `turnCompleted`
    /// without being persisted is fixed: it now lands in chat_log with the meta, so past turns show their cost too.
    #[serde(default)]
    pub usage: Option<TokenUsage>,
    /// True when this turn was cut short by `turn/interrupt`.
    /// Older metas in the DB lack this field — `#[serde(default)]` lets them parse as before.
    #[serde(default)]
    pub interrupted: bool,
    /// How many times the loop guard fired this turn (0 = the model never got stuck
    /// repeating/alternating tool calls). Zero is normal; non-zero means part of this turn's budget went to spinning in place.
    #[serde(default)]
    pub guard_hits: i32,
    /// What the guard told the model when it fired (a human-readable explanation; null otherwise).
    #[serde(default)]
    pub guard_note: Option<String>,
    /// How many provider retries happened this turn (rate limits / transient faults). Zero is the norm.
    #[serde(default)]
    pub retries: i32,
    /// The goal loop's exit (`continuing` / `satisfied` / `impossible` /
    /// `round-limit` / `blocked`). `None` when no goal is set.
    #[serde(default)]
    pub goal_status: Option<String>,
    /// How many rounds this goal has consumed up to this turn. 0 when no goal is set.
    #[serde(default)]
    pub goal_rounds: i32,
    /// How this turn's model was chosen (routing, see route.rs): `auto:reasoning → ds-r1`
    /// or `fallback: …`. `None` in fixed mode and when the chosen one is already active —
    /// the footer stays quiet and only speaks when a choice/fallback actually happened.
    #[serde(default)]
    pub route_reason: Option<String>,
}

// ===========================================================================
// Memory
// ===========================================================================

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct Fact {
    pub id: i32,
    pub subject: String,
    pub content: String,
    /// `user` (told directly) or `consolidation` (distilled from conversations).
    pub source: String,
    /// What this fact is **about**: `user` / `feedback` / `project` / `reference` /
    /// `fact` (fallback). Anything that resists classification lands on `fact`.
    pub kind: String,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct Episode {
    pub id: i32,
    /// ISO 8601 date.
    pub happened_at: String,
    pub summary: String,
    pub created_at: Option<String>,
}

// ===========================================================================
// Sessions / models / config
// ===========================================================================

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionSummary {
    pub id: String,
    /// The name a human gave it (migration 0018); when there is none, the first user message.
    pub title: String,
    pub messages: i32,
    pub started_at: Option<String>,
    pub last_at: Option<String>,
    /// The session's working directory: the **root of everything it may write** (tools refuse
    /// writes outside it). Absent = not chosen; the dashboard shows "选择工作目录".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub cwd: Option<String>,
}

/// A message's role. A **closed set** — deliberately no "unknown" escape hatch.
///
/// If some future `chat_log` really does grow other roles, `session/messages` will **skip**
/// those rows rather than force a value in — so the frontend `switch` can stay exhaustive
/// and "there is a third role to render" shows up at compile time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum MessageRole {
    User,
    Assistant,
}

/// One message in session history: a row of `chat_log`.
///
/// Same conversation, second viewpoint alongside the live stream (`turnStarted` /
/// `textDelta` / `turnCompleted`): the live stream lives only for **this connection**
/// (gone on refresh); this one makes "refresh, switch devices, look back later" possible.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct Message {
    /// The chat_log row number. **Also the cursor**: it is `session/messages`' cursor.
    /// Row numbers over timestamps: both rows of one turn are written in the same second, so timestamps cannot order them.
    pub id: i32,
    pub role: MessageRole,
    pub content: String,
    /// ISO 8601 (UTC). When it was written.
    pub at: String,
    /// This turn's telemetry (gate / tools / iterations / model / usage), **present only on assistant rows**.
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

/// The **read view** of config. Fields are deliberately complete and non-optional: reading it yields the full current state.
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
    /// Working-memory sliding window: only the last N turns go into the prompt.
    pub history_turns: i32,
    pub consolidate_every: i32,
    pub retrieval_top_k: i32,
    pub graph_workflows: bool,
    pub home: String,
}

/// The **write patch** for config. Pass only the fields to change; null means leave as-is.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SettingsPatch {
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
    pub graph_workflows: Option<bool>,
    /// Whole-table replacement of the allowlist (`JOY_EXEC_ALLOW`) — what "remember this command" writes.
    #[ts(optional = nullable)]
    pub exec_allow: Option<Vec<String>>,
}

impl SettingsPatch {
    /// Not a single field to change.
    pub fn is_empty(&self) -> bool {
        let p = self;
        p.max_iterations.is_none()
            && p.max_tokens.is_none()
            && p.history_turns.is_none()
            && p.consolidate_every.is_none()
            && p.retrieval_top_k.is_none()
            && p.graph_workflows.is_none()
    }

    /// Overwrite self with the non-null fields of `newer` — for accumulating patches:
    /// the saved patch is the foundation of the new one, not the other way around.
    pub fn merge_newer(&mut self, newer: &Self) {
        let p = newer;
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
        if p.graph_workflows.is_some() {
            self.graph_workflows = p.graph_workflows;
        }
        if p.exec_allow.is_some() {
            self.exec_allow = p.exec_allow.clone();
        }
    }
}

// ===========================================================================
// Requests / responses
// ===========================================================================

/// One image attached to a turn. `path` is **home-relative** (uploads land in
/// `<home>/uploads/...`); the server reads the bytes and embeds them — the model never
/// sees a path, it sees pixels. Only image files are accepted; other files keep the
/// "path mentioned in text" route.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnAttachment {
    pub path: String,
    /// Client-provided media type hint; the server trusts the file extension over this.
    #[ts(optional = nullable)]
    pub media_type: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnStartParams {
    #[ts(optional = nullable)]
    pub session_id: Option<String>,
    pub message: String,
    /// Whether to stream `textDelta` token by token. On for the dashboard, off for scripts.
    #[ts(optional = nullable)]
    pub stream: Option<bool>,
    /// Working directory for this turn (per-turn override semantics):
    /// must be an existing absolute path (fail-closed admission); relative exec commands
    /// and relative read_file/list_dir paths resolve against it. Default = process cwd (old behavior).
    #[ts(optional = nullable)]
    pub cwd: Option<String>,
    /// Images attached to this message, embedded as image content blocks. Requires a
    /// vision-capable provider; a missing or unreadable file fails the turn up front.
    #[ts(optional = nullable)]
    pub attachments: Option<Vec<TurnAttachment>>,
    /// Pin this turn to a model tier (`fast` / `chat` / `reasoning`). Absent = let the server
    /// decide (task's declared tier, auto routing, or the active profile — in that order).
    /// Machine callers (task API, webhooks) say the tier; the model itself cannot.
    #[ts(optional = nullable)]
    pub tier: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnStartResponse {
    pub turn_id: String,
}

/// The task API's one-shot shape (`POST /api/run`): a whole turn in, one answer out.
///
/// Same request body as `turn/start` (`TurnStartParams`), no stream — the HTTP call returns
/// when the turn finishes. Built for programs (CI, cron, another agent), not for a browser:
/// the dashboard keeps SSE, because a person wants to watch the answer arrive.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct RunResponse {
    pub turn_id: String,
    pub reply: String,
    /// The model that actually answered — the visible proof that a tier pin took effect.
    pub model: String,
    pub provider: String,
    /// Why this model was chosen (`tier:fast → mini`, `auto:reasoning → ds-r1`); absent = no routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_reason: Option<String>,
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
    /// false = this turn already finished; nothing was interrupted.
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
    /// Generated by the server when absent.
    #[ts(optional = nullable)]
    pub session_id: Option<String>,
    /// The name to give it. Absent = unnamed (the list falls back to the first user message).
    #[ts(optional = nullable)]
    pub title: Option<String>,
    /// Working directory to bind it to. Must be an existing absolute path; absent = not chosen.
    #[ts(optional = nullable)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionNewResponse {
    pub session_id: String,
}

/// Read what was said in a session.
///
/// **Why it exists**: without it, the conversation on the page lives only
/// in the browser — a refresh erases it. Also the prerequisite for "switch session".
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionMessagesParams {
    pub session_id: String,
    /// Fetch only messages **older** than this one (paging back through history). Absent = the newest page.
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

/// Messages with the **newest first** — opposite to the conversation's direction,
/// because "paging back" is the point: take the latest page, then follow `nextCursor` into the past.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionMessagesResponse {
    pub data: Vec<Message>,
    /// Present only when older messages exist: pass it as the next cursor. null = the end.
    pub next_cursor: Option<String>,
}

/// Read one session's record. Same shape as a list entry — one type, no second modeling.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionGetParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionGetResponse {
    pub session: SessionSummary,
}

/// Rename a session or rebind its working directory.
///
/// **`null`/absent = leave alone; `""` = clear.** Two different intents must not collapse into
/// one: "I renamed it" and "I never touched the title" are different writes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionUpdateParams {
    pub session_id: String,
    #[ts(optional = nullable)]
    pub title: Option<String>,
    /// Must be an existing absolute path when non-empty (same admission rule as `turn/start`).
    #[ts(optional = nullable)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionUpdateResponse {
    pub session: SessionSummary,
}

/// One turn that is running **right now** (in memory — a finished turn is simply gone).
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnView {
    pub turn_id: String,
    pub session_id: String,
    /// RFC 3339 with offset. The page computes "running for N minutes" from it, so the browser
    /// needs no second clock of its own.
    pub started_at: String,
}

/// Which turns are running. Absent `sessionId` = the whole home (the CLI's "what is happening").
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnListParams {
    #[ts(optional = nullable)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnListResponse {
    pub turns: Vec<TurnView>,
}

/// Add one sentence to a turn that is already running (0.16).
///
/// Not `turn/start`: that one is refused while a turn runs (`SESSION_BUSY`), and for good reason
/// — two turns on one session would interleave their writes. Steering is the supported way to
/// say "and also" without cancelling work that has already happened.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnSteerParams {
    pub session_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnSteerResponse {
    pub turn_id: String,
    /// How many additions this turn has accepted so far, this one included — "queued: 1" reads
    /// as "it will be seen", "queued: 3" as "it is piling up behind two others".
    pub queued: i32,
}

/// Queue one message to run **after** the turn that is currently running (0.16).
///
/// The difference from `turn/steer` is what the person means, not where the text goes: steer is
/// "and also, while you are at it", follow-up is "when you are done". A follow-up becomes its own
/// turn — its own rounds, budget and checkpoint — and runs even after this page is closed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnFollowUpParams {
    pub session_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnFollowUpResponse {
    pub session_id: String,
    /// Position in line: 1 = runs as soon as the current turn ends, 2 = after that one.
    pub queued: i32,
}

/// The human added a sentence to a running turn. Emitted to **every** client of the turn, so a
/// second device sees the addition at the moment it happened instead of on the next reload.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnSteeredNotification {
    pub turn_id: String,
    pub session_id: String,
    pub message: String,
    pub ts: String,
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
    /// Defaults to `user` (said directly by the user). Consolidation passes
    /// `consolidation` when distilling from conversations, so the frontend can tell "you told it" from "it summarized itself".
    #[ts(optional = nullable)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MemoryRememberResponse {
    /// The full fact as written, with id — the frontend can insert it directly, no re-query needed.
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
    /// How many were actually deleted. 0 means there was no memory of this subject to begin with.
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

/// The episode list. **No cursor**: `Episodes::recent` only fetches the most recent N by
/// time desc — storage has no notion of offset. Rather than invent an unusable cursor, it honestly returns null.
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
    /// Fetch only these keys; absent = return everything.
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
    /// The full config after writing, so the frontend can replace its local cache outright.
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

/// Response to `model/profiles`: all configured model profiles + the active one.
// Unit struct: no fields at all, so rename_all does not apply (ts-rs rejects
// rename_all on an empty struct — there are no fields to rename).
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[ts(export_to = "v2/")]
pub struct ModelProfilesParams;

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[ts(export_to = "v2/")]
pub struct UsageSummaryParams;

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[ts(export_to = "v2/")]
pub struct ApprovalLogParams;

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[ts(export_to = "v2/")]
pub struct ScheduleListParams;

/// One row of `schedule/list`: a scheduled job's current state (migration 0012).
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ScheduleStateRow {
    pub name: String,
    /// The last settled trigger minute (the dedup anchor); null if it never ran.
    pub last_fired_minute: Option<String>,
    /// Non-null = the last run was interrupted; on restart catch_up decides whether to re-run or give up.
    pub claimed_minute: Option<String>,
    /// true = at-least-once (re-run), false = at-most-once (give up).
    pub catch_up: bool,
    /// outbox / notify / quiet.
    pub delivery: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ScheduleListResponse {
    /// All jobs sorted by name; an empty array when none was ever registered.
    pub jobs: Vec<ScheduleStateRow>,
}

/// A profile's **view**: unlike the stored copy it contains no secret, only where the secret lives.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ModelProfileView {
    pub id: String,
    /// The provider id in `joyczl-provider` terms.
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub small_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Where the credential lives: `keychain` / `file` / `env:VAR` / `memory` / none (local model).
    /// **A pointer, not the key itself** — the UI uses it to honestly show "where your key is stored".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_source: Option<String>,
    /// Whether this profile's model accepts images.
    /// `null` = follow the vendor's default model (the catalog row).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    /// Whether this profile is currently active.
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ModelProfilesResponse {
    /// The currently active id (null when nothing is configured).
    pub active_id: Option<String>,
    pub profiles: Vec<ModelProfileView>,
}

/// Parameters for `model/save`. `provider` is required only when **creating** —
/// passing only `id` + `make_active` means switch.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ModelSaveParams {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub small_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The key itself. Absent = keep as-is (changing models or switching never requires re-sending).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Use a key from an environment variable (pass a name like `DEEPSEEK_API_KEY`):
    /// Joy only reads it and never copies it into any file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_env: Option<String>,
    /// Whether this model accepts images. Absent = keep the vendor's default (patch
    /// semantics, same as `model`); `true` is how "this vendor's default is text-only,
    /// but *this* model sees" gets declared at the moment the profile is created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    /// Make it active right after saving (default true — the smoothest on first setup).
    #[serde(default)]
    pub make_active: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ModelSaveResponse {
    pub id: String,
    /// Whether this profile is currently active.
    pub active: bool,
    pub active_id: Option<String>,
    /// Where the credential **actually** landed: `keychain` / `file` / `env:VAR` / `memory` / `none`.
    /// Fall back to a plaintext file and say so honestly — never let the user assume the keychain.
    pub key_stored_in: String,
    /// The current model after switching (`provider / model`); null when not fully configured.
    pub current: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ModelRemoveParams {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ModelRemoveResponse {
    pub removed: bool,
    /// When the active profile is removed, active is cleared along with it.
    pub active_id: Option<String>,
}

/// One tier's entry in the routing table. `profile_id` absent = "follow the fallback chain"
/// (`reasoning` → `chat` → active, `fast` → `chat` → active), i.e. what the router does anyway.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct TierRoute {
    /// fast / chat / reasoning.
    pub tier: String,
    /// `null` is a real answer ("follow the chain"), so it is always on the wire — the panel
    /// distinguishes "not configured" from "field missing" by reading this, not by guessing.
    #[serde(default)]
    pub profile_id: Option<String>,
}

/// `model/routes`: the routing table as the panel shows it. Always three rows (the tiers are
/// fixed), each maybe pointing at nothing — an empty table is a valid, honest state.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ModelRoutesResponse {
    /// `fixed` (default, active profile wins) or `auto` (per-turn tier pick).
    pub mode: String,
    pub routes: Vec<TierRoute>,
}

/// `model/routes/save`: write the whole table (no patch semantics — three rows are cheap and
/// "replace the table" cannot leave a half-edited state behind).
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ModelRoutesSaveParams {
    pub mode: String,
    #[serde(default)]
    pub routes: Vec<TierRoute>,
}

/// Response to `usage/summary`: per-model aggregates + totals. In hub mode this is per-user
/// bookkeeping (one home and one state.db per user — the ledger is isolated by person by construction).
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct UsageByModel {
    pub provider: String,
    pub model: String,
    pub turns: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct UsageSummaryResponse {
    pub turns: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub by_model: Vec<UsageByModel>,
}

/// One row of `approval/log`: the complete record of one "ask before running".
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ApprovalLogEntry {
    pub turn_id: String,
    pub request_id: String,
    pub tool: String,
    pub args_preview: String,
    pub reason: String,
    /// One of approved / denied / timeout / voided.
    pub outcome: String,
    pub decided_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct ApprovalLogResponse {
    /// Newest first, capped at 100 entries by the server.
    pub entries: Vec<ApprovalLogEntry>,
}

// ===========================================================================
// Dashboard
// ===========================================================================

/// Everything the dashboard's first screen needs, fetched in one shot.
///
/// It is not the response to any RPC but the payload of the `joy dashboard` HTTP
/// endpoint (`GET /api/data`). **Why it lives in the protocol crate**: the only
/// criterion is that it crosses languages. The browser side type-checks against it, and hand-copying
/// it would drift again — exactly what this whole effort exists to eliminate.
///
/// Fields reuse protocol types directly: the dashboard does no second modeling —
/// it is just a view over these data.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct DashboardData {
    /// ISO 8601, millisecond precision. The page shows "when is this data from".
    pub generated_at: String,
    pub config: SettingsView,
    pub sessions: Vec<SessionSummary>,
    pub facts: Vec<Fact>,
    pub episodes: Vec<Episode>,
    /// The viewed session's todo sheet, when asked for and non-empty. This is the
    /// model's own working sheet (todo_write), shown to the human as the task card.
    #[serde(default)]
    #[ts(optional)]
    pub todos: Option<TodoListView>,
    /// The viewed session's current task (goal upgraded to a persisted entity), when asked for.
    #[serde(default)]
    #[ts(optional)]
    pub task: Option<AgentTaskView>,
    /// What that task actually produced: files that exist, with their sizes (0.14). Always
    /// present (empty when there is no task), so the card needs no branch to render it.
    #[serde(default)]
    pub task_artifacts: Vec<ArtifactView>,
    /// The connector inbox (0.13.0): newest mail plus how many still wait for a person.
    /// Always present (empty when no connector is configured) so the panel needs no branches.
    pub inbox: InboxListView,
    /// Connectors configured on this machine (0.12.0). Read from connectors.json by ops:
    /// it is the same file the app-server registered tools from, on the same machine.
    #[serde(default)]
    pub connectors: Vec<ConnectorStatus>,
    /// Why the connector file could not be read, if it could not — the panel shows this
    /// instead of silently pretending there are no connectors.
    #[serde(default)]
    #[ts(optional)]
    pub connectors_error: Option<String>,
}

/// One configured connector, as far as the dashboard needs to know: which one, whether
/// it can actually send, and how wide the door is (allowlist size).
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ConnectorStatus {
    /// `email` / `feishu`.
    pub id: String,
    /// `false` = draft-only: nothing leaves the machine.
    pub live: bool,
    /// How many entries the recipient allowlist has. `0` means nobody — said, not implied.
    pub allow_recipients: usize,
}

/// One row of the todo sheet. `status` is the wire form of `TodoStatus`
/// (pending / in_progress / completed / cancelled) — a string, not an enum:
/// the sheet is the model's vocabulary, the dashboard only paints it.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TodoItemView {
    pub content: String,
    pub status: String,
}

/// The whole sheet as the human sees it. `revision` is the board's monotonic
/// version — the UI can tell "changed" from "refetched" with it.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TodoListView {
    pub revision: i64,
    pub items: Vec<TodoItemView>,
}

// ===========================================================================
// Notifications (server → client, the full course of one turn)
// ===========================================================================

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnStartedNotification {
    pub turn_id: String,
    pub session_id: String,
    pub user_message: String,
    /// ISO 8601, millisecond precision.
    pub ts: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TextDeltaNotification {
    pub turn_id: String,
    /// A streaming text delta. Present only when `turn/start` passed `stream: true`.
    pub delta: String,
}

/// A "retrying now" notice.
///
/// Retries are **never silent**: the user sees this message, not an inexplicable long pause.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct RetryNotification {
    pub turn_id: String,
    /// Which retry attempt (starting at 1).
    pub attempt: i32,
    /// Why it is retrying (`HTTP 429` / `network error: …`).
    pub reason: String,
    /// How long it waited.
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
    /// The model-supplied arguments; structure depends on the tool.
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
    /// The text the tool returned to the model. Failure details live here too — the turn is not aborted.
    pub output: String,
    pub status: ToolStatus,
    pub duration_ms: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ConsolidationCompletedNotification {
    /// How many new facts this batch distilled; 0 means not due yet or nothing surfaced.
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
    /// Which visit to this node this is (>1 only happens in deliberately designed loops).
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
    /// Which state keys the node wrote back.
    pub keys: Vec<String>,
    /// Error text when the node throws; errors are logged, never rethrown, and the run still finishes normally.
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

/// The full stream of events for one turn.
///
/// `type` is the discriminant — the frontend switches on it, Python uses a pydantic discriminated
/// union. **No failure here should crash the client**: this only describes "what happened".
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(tag = "type")]
#[ts(export_to = "v2/")]
pub enum ServerNotification {
    TurnStarted(TurnStartedNotification),
    TextDelta(TextDeltaNotification),
    GateDecided(GateDecidedNotification),
    /// Retrying now (rate limit / transient fault): **sent every time**, never silent.
    Retry(RetryNotification),
    /// A command is waiting for approval: answer it first, then the turn continues.
    ApprovalRequested(ApprovalRequestedNotification),
    GoalRound(GoalRoundNotification),
    ToolStarted(ToolStartedNotification),
    ToolCompleted(ToolCompletedNotification),
    ConsolidationCompleted(ConsolidationCompletedNotification),
    GraphStarted(GraphStartedNotification),
    GraphNodeStarted(GraphNodeStartedNotification),
    GraphNodeEnded(GraphNodeEndedNotification),
    GraphEnded(GraphEndedNotification),
    /// A checkpoint is being resumed after crash recovery (finished tools are not re-run).
    TurnCheckpointed(TurnCheckpointedNotification),
    /// Someone added a sentence to a turn that was already running (0.16).
    TurnSteered(TurnSteeredNotification),
    TurnResumed(TurnResumedNotification),
    TurnSuspended(TurnSuspendedNotification),
    /// The task behind this turn changed state (created / running / suspended / done…).
    TaskUpdated(TaskUpdatedNotification),
    /// New mail in the connector inbox (inbound, 0.13.0). A knock — the panel pulls details.
    InboxReceived(InboxReceivedNotification),
    /// Boxed: its `TurnMeta` is an order of magnitude bigger than the others, and
    /// notifications are built per event and shipped via channels. ts-rs is transparent to `Box<T>`.
    TurnCompleted(Box<TurnCompletedNotification>),
    Error(ErrorNotification),
}
