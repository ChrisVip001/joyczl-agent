// 由 ts-rs 生成物汇总而来，别手改。
// 生成逻辑：joyczl-protocol/src/export.rs 的 write_consts。
// 清单在 `protocol::v2::methods`。值就是线上传的方法名。
export const METHODS = {
  TURN_START: "turn/start",
  TURN_INTERRUPT: "turn/interrupt",
  APPROVAL_RESPOND: "approval/respond",
  SESSION_LIST: "session/list",
  SESSION_NEW: "session/new",
  SESSION_MESSAGES: "session/messages",
  MEMORY_SEARCH: "memory/search",
  MEMORY_LIST: "memory/list",
  MEMORY_LIST_EPISODES: "memory/list-episodes",
  MEMORY_REMEMBER: "memory/remember",
  MEMORY_FORGET: "memory/forget",
  CONFIG_READ: "config/read",
  CONFIG_WRITE: "config/write",
  MODEL_LIST: "model/list",
} as const;
