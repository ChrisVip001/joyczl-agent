// 由 ts-rs 生成物汇总而来，别手改。
// 生成逻辑：joyczl-protocol/src/export.rs 的 write_consts。
// 清单在 `rpc::codes`。key 跟 Rust 侧常量同名，好让两边 grep 得到同一处。
export const CODES = {
  PARSE_ERROR: -32700,
  INVALID_REQUEST: -32600,
  METHOD_NOT_FOUND: -32601,
  INVALID_PARAMS: -32602,
  INTERNAL_ERROR: -32603,
  PROVIDER_ERROR: -32000,
  TOOL_ERROR: -32001,
  NOT_IMPLEMENTED: -32002,
} as const;
