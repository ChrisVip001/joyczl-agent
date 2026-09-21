//! 代码生成：Rust 类型 → TypeScript + JSON Schema。
//!
//! 只在 `cargo test` 下编译（见 lib.rs 的 derive 切换）。由
//! `scripts/write_schema.py` 驱动那一个被 `#[ignore]` 的测试来触发。

use std::path::{Path, PathBuf};

use crate::protocol::v2::*;
use crate::rpc::*;

/// 仓库里已提交的生成物目录。
pub const SCHEMA_ROOT_ENV: &str = "JOYCZL_SCHEMA_ROOT";
/// 本次导出写到哪里 —— 由 scripts/write_schema.py 决定：
/// 正常生成时等于 SCHEMA_ROOT，`--check` 时是临时目录（比对完就删）。
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

/// 需要导出的全部类型 —— **唯一清单**。
///
/// 新增协议类型时在这里加一行，TypeScript 和 JSON Schema 就都跟上了。
/// 忘了加不会报错，只会让那一类静默消失，所以这里刻意集中在一处，
/// 好让 review 时只盯这一个 macro。
macro_rules! protocol_types {
    ($cb:ident) => {
        $cb!(
            // ---- JSON-RPC 信封
            RequestId,
            JsonRpcRequest,
            JsonRpcResponse,
            JsonRpcError,
            ErrorObject,
            JsonRpcNotification,
            JsonRpcMessage,
            // ---- 通用
            TokenUsage,
            GateDecisionKind,
            GateDecision,
            GraphRouteKind,
            GraphInfo,
            ToolStatus,
            ToolCallRecord,
            TurnMeta,
            // ---- 会话历史
            MessageRole,
            Message,
            // ---- 记忆
            Fact,
            Episode,
            // ---- 会话 / 模型 / 配置
            SessionSummary,
            ModelInfo,
            SettingsView,
            SettingsPatch,
            // ---- 请求 / 应答
            TurnStartParams,
            TurnStartResponse,
            TurnInterruptParams,
            TurnInterruptResponse,
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
            // ---- 驾驶舱
            DashboardData,
            // ---- 通知
            TurnStartedNotification,
            TextDeltaNotification,
            GateDecidedNotification,
            RetryNotification,
            ToolStartedNotification,
            ToolCompletedNotification,
            ConsolidationCompletedNotification,
            GraphStartedNotification,
            GraphNodeStartedNotification,
            GraphNodeEndedNotification,
            GraphEndedNotification,
            TurnCompletedNotification,
            ErrorNotification,
            ServerNotification,
        )
    };
}

/// 导出 TypeScript 到 `out_dir`。
///
/// ts-rs 会顺着类型依赖把被引用的类型一起写出来，所以每个类型都得带
/// `#[ts(export_to = "v2/")]`——漏了的类型会被塞到默认目录，而不是报错。
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

/// 常量也生成一份：错误码、方法名。
///
/// 它们都是对线协议的一部分，却没进 `protocol_types!`（那是一张**类型**清单，
/// 常量进不去）。而「另一种语言里手抄一份」正是这个仓库到处在避免的事 ——
/// 改名只改一边不会报错，只会在运行时对不上。所以直接从常量读出值写出去，
/// 清单只此一份。
///
/// `as const` 让每个字段都是字面量类型：TS 侧能拿它当计算属性名用
/// （见 client.ts 的 `Methods`），于是方法名也不可能写错。
///
/// **为什么是 `.mts` 而不是 `.ts`**：这两个文件跟旁边那 63 个不同，它们有真的
/// 运行时导出。ts-rs 写的类型文件全在 `import type` 之间打转，无扩展名相对导入
/// 永远不会进运行时，所以 `.ts` 就够；而这里有真值，必须把「它是 ESM」说明白，
/// 否则 tsc 按 CommonJS 推断会直接对着 `export const` 报 TS1287。
///
/// 用扩展名而不是往目录里放一个 `{"type":"module"}`：后者会波及整个 `v2/`，
/// 把 ts-rs 那些无扩展名导入一口气判成违规。**别去加那个 package.json。**
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

/// 错误码清单：名字 → 数值。**只此一份** —— TS 的 `codes.mts` 和 JSON 的
/// `consts.json` 都从这儿出，Python 再从 JSON 生成自己的 `CODES`。
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

/// 方法名清单：名字 → 线上传的字符串。同上，只此一份。
fn method_entries() -> Vec<(&'static str, &'static str)> {
    use crate::protocol::v2::methods as m;
    vec![
        ("TURN_START", m::TURN_START),
        ("TURN_INTERRUPT", m::TURN_INTERRUPT),
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
    ]
}

/// 常量也导出一份 JSON。
///
/// JSON Schema 只装得下**类型**，装不下**值** —— 错误码和方法名这两组常量
/// 就这么漏在 TS 里了。Python SDK 要拿它们（`joy client.memory_remember()`
/// 得知道线上传的是 `memory/remember`），而在 Python 里手抄一份正是这个仓库
/// 到处在避免的事：改名只改一边不会报错，只会在运行时对不上。
///
/// 所以补这一份 JSON 作为中间形态，清单仍然只有上面两处。
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

/// 拼一个 `export const X = { … } as const;` 模块。
fn render_const_module(
    object: &str,
    source: &str,
    note: &str,
    entries: &[(&str, String)],
) -> String {
    let mut text = format!(
        "// 由 ts-rs 生成物汇总而来，别手改。\n\
         // 生成逻辑：joyczl-protocol/src/export.rs 的 write_consts。\n\
         // 清单在 {source}。{note}\n"
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

/// 再写一个 `v2/index.ts`，把全部类型重导成一个入口。
///
/// 生成出来的每个文件只 `import type "./X"`，TS 侧想用就得逐个文件引。
/// 手写一份清单迟早会漏，而漏了不报错 —— 只是类型悄悄少一个，跟
/// `protocol_types!` 漏一行是同一类坑。所以清单只留一份，这里生成。
///
/// `stringify!` 拿到的就是类型名，也正好是 ts-rs 用的文件名：这些类型都是
/// 无泛型、无重命名的平名字。真出了重命名，漂移检查会在那一次 diff 里现形。
fn write_barrel(out_dir: &Path) -> Result<(), String> {
    macro_rules! collect_names {
        ($($ty:ty),* $(,)?) => { vec![$(stringify!($ty)),*] };
    }
    let names = protocol_types!(collect_names);

    let mut barrel = String::from(
        "// 由 ts-rs 生成物汇总而来，别手改。\n\
         // 生成逻辑：joyczl-protocol/src/export.rs 的 write_barrel。\n",
    );
    for name in names {
        barrel.push_str(&format!("export type * from \"./{name}\";\n"));
    }

    let path = out_dir.join("v2").join("index.ts");
    std::fs::write(&path, barrel).map_err(|e| format!("{}: {e}", path.display()))
}

/// 生成一份打包好的 JSON Schema，供 datamodel-code-generator 产出 pydantic。
///
/// 所有类型并列放进 `definitions`，而不是做一个巨大的根对象 ——
/// 这样 Python 侧生成出来的是一组平级模型，不是 `V2.token_usage.fact` 这种嵌套。
pub fn generate_json_schema() -> serde_json::Value {
    use schemars::gen::{SchemaGenerator, SchemaSettings};

    let mut gen = SchemaGenerator::new(SchemaSettings::draft07());
    let mut defs = serde_json::Map::new();

    macro_rules! add_each {
        ($($ty:ty),* $(,)?) => { $(
            let name = <$ty as schemars::JsonSchema>::schema_name();
            let root = gen.root_schema_for::<$ty>();
            // 该类型引用到的依赖先进 definitions …
            for (k, v) in &root.definitions {
                if let Ok(value) = serde_json::to_value(v) {
                    defs.entry(k.clone()).or_insert(value);
                }
            }
            // … 再放它自己。root.schema 常常是 `#/definitions/<Name>` 的一个
            // $ref，这时上面已经填过了，or_insert 会跳过，避免自引用。
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

/// 生成物里至少该有这么多类型定义。防止某天 macro 清单被改空而没人发现。
pub const EXPECTED_MIN_DEFS: usize = 40;
