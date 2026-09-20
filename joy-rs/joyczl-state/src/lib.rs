//! `joyczl-state` —— Joy 记得住的一切，都在这一个 SQLite 文件里。
//!
//! 三语言工程里这条 crate 是**独占**的：只有 app-server 打开 state.db。
//! 前端、网关、SDK 想读写记忆，一律走 JSON-RPC，不许自己开数据库连接。
//! 于是「跨线程共享 sqlite 连接」这类问题从根上不存在 ——
//! 连接由单一进程持有，天然没有并发争用。
//!
//! 分层：
//!   * `db`        打开 / 迁移
//!   * `facts`     语义记忆（FTS5 检索）
//!   * `episodes`  情景记忆
//!   * `chat`      原始对话日志 + 会话（会话只是 chat_log 上的一个标签）
//!   * `fts`       把人类输入变成安全的 FTS5 查询串

pub mod calendar;
pub mod chat;
pub mod conformance;
pub mod db;
pub mod episodes;
pub mod facts;
pub mod fts;
pub mod store;

pub use calendar::{Calendar, CalendarRow};
pub use chat::{Chat, MessageRow, SessionRow};
pub use db::open;
pub use episodes::{EpisodeRow, Episodes};
pub use facts::{FactRow, Facts};
pub use store::{EpisodicStore, SemanticStore};

#[cfg(test)]
#[path = "state_tests.rs"]
mod state_tests;

#[cfg(test)]
#[path = "conformance_tests.rs"]
mod conformance_tests;
