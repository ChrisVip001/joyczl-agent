//! Scheduling, in one place: what a job is, when it fires, and where it is declared.
//!
//! Three callers share this crate, and they have to agree on all three questions:
//!
//! * `joyczl-cli`'s `joy schedule` — fires a job when its cron matches the current minute;
//! * the dashboard's capability center — shows the next fire and edits `schedules.json`;
//! * the `schedule_job` / `list_schedules` / `cancel_schedule` tools — let the model create
//!   one from a sentence instead of asking the human to hand-edit a file.
//!
//! Agreement is the whole point. Two copies of "what a valid job is" drift into the panel
//! saying 08:00 while nothing fires at 08:00, or into the model writing a job the scheduler
//! silently skips.

pub mod cron;
pub mod declarations;

pub use cron::{matches, next_after, next_run, valid};
pub use declarations::{read, remove, save, FileJob, JobBody, DELIVERIES};
