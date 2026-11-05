//! Code generation: Rust types → TypeScript + JSON Schema.
//!
//! Only compiled under `cargo test` (see the derive switch in lib.rs). Driven
//! by `scripts/write_schema.py`, which drives the single `#[ignore]`d test.

use std::path::{Path, PathBuf};

use crate::protocol::v2::*;
use crate::rpc::*;

/// The checked-in generated-artifacts directory in the repo.
pub const SCHEMA_ROOT_ENV: &str = "JOYCZL_SCHEMA_ROOT";
/// Where this export writes — decided by scripts/write_schema.py:
/// equal to SCHEMA_ROOT normally; a temp dir under `--check` (removed after comparing).
pub const SCHEMA_OUT_ENV: &str = "JOYCZL_SCHEMA_OUT";

pub fn schema_root() -> PathBuf {
    std::env::var(SCHEMA_ROOT_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("schema"))
}

pub fn schema_out() -> PathBuf {
    std::env::var(SCHEMA_OUT_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| schema_root())
}

/// Every type that needs exporting — **the single list**.
///
/// Add one line here for a new protocol type and TypeScript and JSON Schema both follow.
/// Forgetting is silent — the type just quietly disappears — so the list is deliberately
/// kept in one place, and review only has to watch this one macro.
macro_rules! protocol_types {
    ($cb:ident) => {
        $cb!(
            // ---- JSON-RPC envelope
            RequestId,
            JsonRpcRequest,
            JsonRpcResponse,
            JsonRpcError,
            ErrorObject,
            JsonRpcNotification,
            JsonRpcMessage,
            // ---- Common
            TokenUsage,
            GateDecisionKind,
            GateDecision,
            GraphRouteKind,
            GraphInfo,
            ToolStatus,
            ToolCallRecord,
            TurnMeta,
            // ---- Session history
            MessageRole,
            Message,
            // ---- Memory
            Fact,
            Episode,
            // ---- Sessions / models / config
            SessionSummary,
            ModelInfo,
            SettingsView,
            SettingsPatch,
            // ---- Requests / responses
            TurnStartParams,
            TurnAttachment,
            TurnStartResponse,
            TurnInterruptParams,
            TurnInterruptResponse,
            ApprovalRespondParams,
            ApprovalRespondResponse,
            GoalSetParams,
            GoalSetResponse,
            ModelProfilesParams,
            ModelProfilesResponse,
            ModelProfileView,
            ModelSaveParams,
            ModelSaveResponse,
            ModelRemoveParams,
            ModelRemoveResponse,
            UsageSummaryParams,
            UsageByModel,
            UsageSummaryResponse,
            ApprovalLogParams,
            ApprovalLogEntry,
            ApprovalLogResponse,
            ScheduleListParams,
            ScheduleStateRow,
            ScheduleListResponse,
            SessionListParams,
            SessionListResponse,
            SessionNewParams,
            SessionNewResponse,
            SessionMessagesParams,
            SessionMessagesResponse,
            MemorySearchParams,
            MemorySearchResponse,
            MemoryListParams,
            MemoryListResponse,
            MemoryListEpisodesParams,
            MemoryListEpisodesResponse,
            MemoryRememberParams,
            MemoryRememberResponse,
            MemoryForgetParams,
            MemoryForgetResponse,
            ConfigReadParams,
            ConfigReadResponse,
            ConfigWriteParams,
            ConfigWriteResponse,
            ModelListParams,
            ModelListResponse,
            // ---- Dashboard
            DashboardData,
            ConnectorStatus,
            TodoItemView,
            TodoListView,
            SessionViewParams,
            // ---- Notifications
            TurnStartedNotification,
            TextDeltaNotification,
            GateDecidedNotification,
            RetryNotification,
            ApprovalRequestedNotification,
            GoalRoundNotification,
            ToolStartedNotification,
            ToolCompletedNotification,
            ConsolidationCompletedNotification,
            GraphStartedNotification,
            GraphNodeStartedNotification,
            GraphNodeEndedNotification,
            GraphEndedNotification,
            TurnCompletedNotification,
            TurnCheckpointedNotification,
            TurnResumedNotification,
            TurnSuspendedNotification,
            // ---- Long tasks
            AgentTaskView,
            TaskCreateParams,
            TaskEnvelope,
            TaskListParams,
            TaskListResponse,
            TaskViewResponse,
            TaskActionParams,
            TaskUpdatedNotification,
            ErrorNotification,
            ServerNotification,
        )
    };
}

/// Export TypeScript into `out_dir`.
///
/// ts-rs writes referenced types along the dependency graph, so every type must carry
/// `#[ts(export_to = "v2/")]` — a missed type lands in the default directory instead of erroring.
pub fn generate_ts(out_dir: &Path) -> Result<(), String> {
    std::env::set_var("TS_RS_EXPORT_DIR", out_dir);

    macro_rules! export_each {
        ($($ty:ty),* $(,)?) => { $(
            <$ty as ts_rs::TS>::export()
                .map_err(|e| format!("{}: {e}", stringify!($ty)))?;
        )* };
    }
    protocol_types!(export_each);
    write_barrel(out_dir)?;
    write_consts(out_dir)
}

/// Constants are generated too: error codes, method names.
///
/// They are part of the wire protocol too, but never entered `protocol_types!` (that is a **type**
/// list; constants cannot get in). And "hand-copying into another language" is exactly what this
/// repo avoids everywhere — renaming one side errors nothing, it just mismatches at runtime. So the
/// values are read straight from the constants and written out; the list exists once.
///
/// `as const` makes every field a literal type: the TS side can use them as computed
/// property names (see `Methods` in client.ts), so method names cannot be typo'd either.
///
/// **Why `.mts` and not `.ts`**: unlike the 63 files beside them, these two have real
/// runtime exports. The type files ts-rs writes stay within `import type`, so extensionless
/// relative imports never reach runtime and `.ts` suffices; here there are real values, so "this is ESM"
/// must be explicit, or tsc infers CommonJS and throws TS1287 right at the `export const`.
///
/// Use the extension instead of dropping a `{"type":"module"}` into the directory: that would affect all of `v2/`,
/// flagging ts-rs's extensionless imports as violations wholesale. **Do not add that package.json.**
fn write_consts(out_dir: &Path) -> Result<(), String> {
    let codes_module = render_const_module(
        "CODES",
        "`rpc::codes`",
        "key 跟 Rust 侧常量同名，好让两边 grep 得到同一处。",
        &code_entries()
            .into_iter()
            .map(|(key, value)| (key, value.to_string()))
            .collect::<Vec<_>>(),
    );

    let methods_module = render_const_module(
        "METHODS",
        "`protocol::v2::methods`",
        "值就是线上传的方法名。",
        &method_entries()
            .into_iter()
            .map(|(key, value)| (key, format!("\"{value}\"")))
            .collect::<Vec<_>>(),
    );

    write_const_ts(out_dir, "codes", &codes_module)?;
    write_const_ts(out_dir, "methods", &methods_module)
}

/// Error-code list: name → value. **The only copy** — TS `codes.mts` and JSON
/// `consts.json` both come from here, and Python generates its own `CODES` from the JSON.
fn code_entries() -> Vec<(&'static str, i32)> {
    use crate::rpc::codes;
    vec![
        ("PARSE_ERROR", codes::PARSE_ERROR),
        ("INVALID_REQUEST", codes::INVALID_REQUEST),
        ("METHOD_NOT_FOUND", codes::METHOD_NOT_FOUND),
        ("INVALID_PARAMS", codes::INVALID_PARAMS),
        ("INTERNAL_ERROR", codes::INTERNAL_ERROR),
        ("PROVIDER_ERROR", codes::PROVIDER_ERROR),
        ("TOOL_ERROR", codes::TOOL_ERROR),
        ("NOT_IMPLEMENTED", codes::NOT_IMPLEMENTED),
    ]
}

/// Method-name list: name → the wire string. Same as above, the only copy.
fn method_entries() -> Vec<(&'static str, &'static str)> {
    use crate::protocol::v2::methods as m;
    vec![
        ("TURN_START", m::TURN_START),
        ("TURN_INTERRUPT", m::TURN_INTERRUPT),
        ("APPROVAL_RESPOND", m::APPROVAL_RESPOND),
        ("GOAL_SET", m::GOAL_SET),
        ("GOAL_VIEW", m::GOAL_VIEW),
        ("TODO_VIEW", m::TODO_VIEW),
        ("TASK_CREATE", m::TASK_CREATE),
        ("TASK_LIST", m::TASK_LIST),
        ("TASK_VIEW", m::TASK_VIEW),
        ("TASK_SUSPEND", m::TASK_SUSPEND),
        ("TASK_RESUME", m::TASK_RESUME),
        ("TASK_CANCEL", m::TASK_CANCEL),
        ("SESSION_LIST", m::SESSION_LIST),
        ("SESSION_NEW", m::SESSION_NEW),
        ("SESSION_MESSAGES", m::SESSION_MESSAGES),
        ("MEMORY_SEARCH", m::MEMORY_SEARCH),
        ("MEMORY_LIST", m::MEMORY_LIST),
        ("MEMORY_LIST_EPISODES", m::MEMORY_LIST_EPISODES),
        ("MEMORY_REMEMBER", m::MEMORY_REMEMBER),
        ("MEMORY_FORGET", m::MEMORY_FORGET),
        ("CONFIG_READ", m::CONFIG_READ),
        ("CONFIG_WRITE", m::CONFIG_WRITE),
        ("MODEL_LIST", m::MODEL_LIST),
        ("MODEL_PROFILES", m::MODEL_PROFILES),
        ("MODEL_SAVE", m::MODEL_SAVE),
        ("MODEL_REMOVE", m::MODEL_REMOVE),
        ("USAGE_SUMMARY", m::USAGE_SUMMARY),
        ("APPROVAL_LOG", m::APPROVAL_LOG),
        ("SCHEDULE_LIST", m::SCHEDULE_LIST),
    ]
}

/// The constants are also exported as JSON.
///
/// JSON Schema holds **types** but not **values** — so the error-code and
/// method-name constants slipped through only in TS. The Python SDK needs them
/// (`joy client.memory_remember()` must know the wire string is `memory/remember`),
/// and hand-copying into Python is exactly what this repo avoids: it would only mismatch at runtime.
///
/// So this JSON is the intermediate form; the list still lives only in the two places above.
pub fn generate_consts_json() -> serde_json::Value {
    let collect = |entries: Vec<(&'static str, serde_json::Value)>| {
        entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect::<serde_json::Map<String, serde_json::Value>>()
    };

    let codes = collect(
        code_entries()
            .into_iter()
            .map(|(key, value)| (key, serde_json::json!(value)))
            .collect(),
    );
    let methods = collect(
        method_entries()
            .into_iter()
            .map(|(key, value)| (key, serde_json::json!(value)))
            .collect(),
    );

    serde_json::json!({ "codes": codes, "methods": methods })
}

/// Assemble an `export const X = { … } as const;` module.
fn render_const_module(
    object: &str,
    source: &str,
    note: &str,
    entries: &[(&str, String)],
) -> String {
    let mut text = format!(
        "// Assembled from the ts-rs generated files — do not edit by hand.\n\
         // Generation logic: write_consts in joyczl-protocol/src/export.rs.\n\
         // Inventory at {source}.{note}\n"
    );
    text.push_str(&format!("export const {object} = {{\n"));
    for (key, value) in entries {
        text.push_str(&format!("  {key}: {value},\n"));
    }
    text.push_str("} as const;\n");
    text
}

fn write_const_ts(out_dir: &Path, name: &str, text: &str) -> Result<(), String> {
    let path = out_dir.join("v2").join(format!("{name}.mts"));
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Also write a `v2/index.ts` re-exporting every type through one entry point.
///
/// Each generated file only does `import type "./X"`, so the TS side imports file
/// by file. A hand-written list will eventually miss one, and a miss does not error —
/// a type just vanishes — same trap as a missing `protocol_types!` line. One list, generated here.
///
/// What `stringify!` yields is the type name, exactly the file name ts-rs uses:
/// flat names — no generics, no renames. A real rename surfaces in that diff via the drift check.
fn write_barrel(out_dir: &Path) -> Result<(), String> {
    macro_rules! collect_names {
        ($($ty:ty),* $(,)?) => { vec![$(stringify!($ty)),*] };
    }
    let names = protocol_types!(collect_names);

    let mut barrel = String::from(
        "// Assembled from the ts-rs generated files — do not edit by hand.\n\
         // Generation logic: write_barrel in joyczl-protocol/src/export.rs.\n",
    );
    for name in names {
        barrel.push_str(&format!("export type * from \"./{name}\";\n"));
    }

    let path = out_dir.join("v2").join("index.ts");
    std::fs::write(&path, barrel).map_err(|e| format!("{}: {e}", path.display()))
}

/// Generate one bundled JSON Schema for datamodel-code-generator to produce pydantic.
///
/// All types sit side by side in `definitions` instead of one giant root object —
/// so the Python side gets a set of sibling models, not `V2.token_usage.fact`-style nesting.
pub fn generate_json_schema() -> serde_json::Value {
    use schemars::gen::{SchemaGenerator, SchemaSettings};

    let mut gen = SchemaGenerator::new(SchemaSettings::draft07());
    let mut defs = serde_json::Map::new();

    macro_rules! add_each {
        ($($ty:ty),* $(,)?) => { $(
            let name = <$ty as schemars::JsonSchema>::schema_name();
            let root = gen.root_schema_for::<$ty>();
            // Dependencies this type references go into definitions first …
            for (k, v) in &root.definitions {
                if let Ok(value) = serde_json::to_value(v) {
                    defs.entry(k.clone()).or_insert(value);
                }
            }
            // … then itself. root.schema is often a $ref to `#/definitions/<Name>`; by then
            // the slot is filled above and or_insert skips it, avoiding self-reference.
            if let Ok(value) = serde_json::to_value(&root.schema) {
                defs.entry(name).or_insert(value);
            }
        )* };
    }
    protocol_types!(add_each);

    serde_json::json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "JoyProtocolV2",
        "definitions": defs,
    })
}

/// Minimum type definitions the artifacts must contain; an emptied macro list cannot slip by.
pub const EXPECTED_MIN_DEFS: usize = 40;
