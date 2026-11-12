// Assembled from the ts-rs generated files — do not edit by hand.
// Generation logic: write_consts in joyczl-protocol/src/export.rs.
// Inventory at `rpc::codes`.the keys match the Rust constants, so one grep finds both sides.
export const CODES = {
  PARSE_ERROR: -32700,
  INVALID_REQUEST: -32600,
  METHOD_NOT_FOUND: -32601,
  INVALID_PARAMS: -32602,
  INTERNAL_ERROR: -32603,
  PROVIDER_ERROR: -32000,
  TOOL_ERROR: -32001,
  NOT_IMPLEMENTED: -32002,
  SESSION_BUSY: -32003,
} as const;
