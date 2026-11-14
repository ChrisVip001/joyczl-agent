//! `joyczl-state` — everything Joy remembers lives in this one SQLite file.
//!
//! The app-server runtime owns database access; browser, gateway, and SDK clients use
//! JSON-RPC. Embedded runtimes can still be started in separate processes (CLI, scheduler,
//! or MCP), so SQLite WAL and connection-level busy timeouts remain necessary. Production execution startup takes a shared OS ownership lock before recovery.
//! Memory-only state users still rely on SQLite transactions; this crate does not acquire that lock.
//!
//! Layers:
//!   * `db`        open / migrate
//!   * `ledger`    the append-only record of every turn (the rebuild source)
//!   * `facts`     semantic memory (FTS5 retrieval)
//!   * `episodes`  episodic memory
//!   * `chat`      the conversation *view* + sessions (a session is just a tag on chat_log)
//!   * `fts`       turn human input into a safe FTS5 query string

pub mod approvals;
pub mod artifacts;
pub mod calendar;
pub mod chat;
pub mod checkpoints;
pub mod completions;
pub mod conformance;
pub mod db;
pub mod dispatch;
pub mod episodes;
pub mod events;
pub mod external_waits;
pub mod facts;
pub mod fts;
pub mod human_pauses;
pub mod inbox;
pub mod jobs;
pub mod ledger;
pub mod operations;
pub mod plans;
pub mod queue;
pub mod runs;
pub mod schedule_completions;
pub mod schedules;
pub mod steps;
pub mod store;
pub mod tasks;
pub mod usage;
pub mod verification;
pub mod waits;

pub use approvals::{ApprovalLogRow, Approvals};
pub use artifacts::{ArtifactRow, Artifacts};
pub use calendar::{Calendar, CalendarRow};
pub use chat::{Chat, MessageRow, SessionRow};
pub use checkpoints::{CheckpointRow, Checkpoints};
pub use db::open;
pub use episodes::{EpisodeRow, Episodes};
pub use events::{EventRow, Events};
pub use facts::{FactMeta, FactRow, Facts};
pub use inbox::{Inbox, InboxItem};
pub use jobs::{JobRow, Jobs};
pub use ledger::BackfillReport;
pub use ledger::{EntryRow, Ledger, NewEntry, TurnRow};
pub use plans::Plans;
pub use queue::{Queue, QueuedMessage};
pub use runs::{RunRow, RunStatus, Runs};
pub use schedules::{ScheduleRow, Schedules};
pub use steps::{NewStep, StepRow, Steps};
pub use store::{EpisodicStore, SemanticStore};
pub use tasks::{NewTask, TaskRow, TaskStatus, Tasks};
pub use usage::{Usage, UsageByModel, UsageSummary};

#[cfg(test)]
#[path = "state_tests.rs"]
mod state_tests;

#[cfg(test)]
#[path = "conformance_tests.rs"]
mod conformance_tests;

#[cfg(test)]
mod dispatch_tests;

#[cfg(test)]
mod completion_tests;

#[cfg(test)]
mod schedule_completion_tests;

#[cfg(test)]
mod checkpoint_round_tests;

#[cfg(test)]
mod memory_edit_tests;

#[cfg(test)]
mod external_wait_tests;

#[cfg(test)]
mod human_pause_tests;

#[cfg(test)]
mod operation_tests;

#[cfg(test)]
mod verification_tests;
