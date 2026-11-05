//! `joyczl-state` — everything Joy remembers lives in this one SQLite file.
//!
//! In the three-language project this crate is **exclusive**: only the app-server opens
//! state.db. Frontend, gateway and SDK go through JSON-RPC only and may not open their own
//! database connections. So "sharing a sqlite connection across threads" does not exist at
//! the root — one process holds the connection; no concurrency contention by nature.
//!
//! Layers:
//!   * `db`        open / migrate
//!   * `facts`     semantic memory (FTS5 retrieval)
//!   * `episodes`  episodic memory
//!   * `chat`      raw conversation log + sessions (a session is just a tag on chat_log)
//!   * `fts`       turn human input into a safe FTS5 query string

pub mod approvals;
pub mod calendar;
pub mod chat;
pub mod checkpoints;
pub mod conformance;
pub mod db;
pub mod episodes;
pub mod facts;
pub mod fts;
pub mod inbox;
pub mod jobs;
pub mod schedules;
pub mod store;
pub mod tasks;
pub mod usage;

pub use approvals::{ApprovalLogRow, Approvals};
pub use calendar::{Calendar, CalendarRow};
pub use chat::{Chat, MessageRow, SessionRow};
pub use checkpoints::{CheckpointRow, Checkpoints};
pub use db::open;
pub use episodes::{EpisodeRow, Episodes};
pub use facts::{FactRow, Facts};
pub use inbox::{Inbox, InboxItem};
pub use jobs::{JobRow, Jobs};
pub use schedules::{ScheduleRow, Schedules};
pub use store::{EpisodicStore, SemanticStore};
pub use tasks::{TaskRow, Tasks};
pub use usage::{Usage, UsageByModel, UsageSummary};

#[cfg(test)]
#[path = "state_tests.rs"]
mod state_tests;

#[cfg(test)]
#[path = "conformance_tests.rs"]
mod conformance_tests;
