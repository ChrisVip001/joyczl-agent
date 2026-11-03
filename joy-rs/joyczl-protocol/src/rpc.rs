//! JSON-RPC 2.0 envelope.
//!
//! The app-server speaks JSON-RPC over stdio: the Rust process owns loop /
//! memory / tools / state.db; every frontend (CLI, dashboard, TS gateway, Python
//! SDK) is its client, so state.db lives in one process — cross-thread SQLite issues vanish.

use serde::{Deserialize, Serialize};

use crate::{JsonSchema, TS};

pub const JSONRPC_VERSION: &str = "2.0";

/// Request id: JSON-RPC allows a number or a string.
///
/// The number arm is `i32`, not `u64`: ts-rs maps `u64` to TypeScript `bigint`,
/// while `JSON.parse` yields `number` — the client receives `1` typed as
/// `bigint`: the kind of bug that never errors but only fails at runtime. Only `i32`
/// crosses language boundaries — the same rule as v2.rs.
// Hash is not part of the wire protocol; it is for client use: every outgoing
// request takes a row in the "id → who awaits the reply" table, so it must be a HashMap key.
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
    /// Shaped like `turn/start`, `<resource>/<method>`, resource in singular.
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
    /// The concrete shape depends on method; decoders narrow it to the matching `*Response` type themselves.
    #[ts(type = "unknown")]
    pub result: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct JsonRpcError {
    pub jsonrpc: String,
    /// null when the id cannot be parsed (e.g. the request itself is bad JSON).
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

/// Server-initiated push (on SSE / stdio this is simply frame after frame of events).
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

/// Any frame the server sends back: a response or an error.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(untagged)]
#[ts(export_to = "v2/")]
pub enum JsonRpcMessage {
    Response(JsonRpcResponse),
    Error(JsonRpcError),
}

/// The standard error codes, plus Joy's own range.
pub mod codes {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
    /// Model / provider failure (invalid key, rate limit, unknown model…)
    pub const PROVIDER_ERROR: i32 = -32000;
    /// Tool execution failed; the error goes back to the model as text, without aborting the turn.
    pub const TOOL_ERROR: i32 = -32001;
    /// The method exists in the protocol but is not implemented yet. Deliberately
    /// distinct from METHOD_NOT_FOUND: "not built yet" and "you typo'd it" differ completely.
    pub const NOT_IMPLEMENTED: i32 = -32002;
}
