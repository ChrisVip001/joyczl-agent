//! JSON-RPC 2.0 信封。
//!
//! app-server 用 JSON-RPC over stdio 通信：Rust 进程持有 loop / memory / tools /
//! state.db，所有前端（CLI、dashboard、TS 网关、Python SDK）都是它的客户端。
//! 这样 state.db 只存在于一个进程里，跨线程共享 SQLite 的那类问题从根上消失。

use serde::{Deserialize, Serialize};

use crate::{JsonSchema, TS};

pub const JSONRPC_VERSION: &str = "2.0";

/// 请求 id：JSON-RPC 允许数字或字符串。
///
/// 数字这一支是 `i32` 而不是 `u64`：`u64` 会被 ts-rs 映射成 TypeScript 的
/// `bigint`，而 `JSON.parse` 出来的是 `number` —— 客户端拿到 `1` 却被告知类型是
/// `bigint`，这是那种不报错、只在运行时对不上的 bug。跨语言边界只用 `i32`，
/// 跟 v2.rs 的规矩是同一条。
// Hash 不是线协议的一部分，是客户端要用的：发出去的每条请求都得在
// 「id → 等应答的那个人」表里占一行，所以它得能当 HashMap 的 key。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(untagged)]
#[ts(export_to = "v2/")]
pub enum RequestId {
    Number(i32),
    String(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: RequestId,
    /// 形如 `turn/start`、`<resource>/<method>`，resource 用单数。
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    #[ts(type = "unknown")]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: RequestId,
    /// 具体结构由 method 决定，解码方按 `*Response` 类型自行 narrow。
    #[ts(type = "unknown")]
    pub result: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct JsonRpcError {
    pub jsonrpc: String,
    /// 解析不了 id 时（比如请求本身就是坏 JSON）为 null。
    pub id: Option<RequestId>,
    pub error: ErrorObject,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ErrorObject {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    #[ts(type = "unknown")]
    pub data: Option<serde_json::Value>,
}

/// 服务端主动推送（SSE / stdio 上就是一帧一帧的事件）。
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    #[ts(type = "unknown")]
    pub params: Option<serde_json::Value>,
}

/// 服务端回的任意一帧：应答或错误。
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(untagged)]
#[ts(export_to = "v2/")]
pub enum JsonRpcMessage {
    Response(JsonRpcResponse),
    Error(JsonRpcError),
}

/// 标准错误码，外加 Joy 自己的一段。
pub mod codes {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
    /// 模型 / provider 出错（key 无效、限流、模型不存在…）
    pub const PROVIDER_ERROR: i32 = -32000;
    /// 工具执行失败；错误作为文本回给模型，不中断 turn。
    pub const TOOL_ERROR: i32 = -32001;
    /// 方法在协议里存在，但当前阶段还没实现。刻意区别于 METHOD_NOT_FOUND：
    /// 「还没做」和「你拼错了」对调用方是完全不同的两件事。
    pub const NOT_IMPLEMENTED: i32 = -32002;
}
