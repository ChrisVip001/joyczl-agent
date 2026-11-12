//! Joy's deterministic agent core — the part that answers "is it really done" without asking a
//! model.
//!
//! Three modules, one rule: **facts decide, opinions may only supplement**.
//!
//! * [`verify`] — the acceptance checks (`fileExists` / `fileContains` / `toolsOk`): a `stat`, a
//!   read, one SQL count. No network, no model call.
//! * [`observe`] — what a round actually did: one step per tool call, deliverables that really
//!   exist, and the contradiction when a tool claims a write that left no file.
//! * [`plan`] — the optional execution plan (`JOY_PLAN`): stored in the step ledger, injected as
//!   text, never an executor.
//! * [`paths`] — the containment rule both the write tools and the verifier use (one
//!   implementation, so "inside the workspace" cannot mean two different things).
//!
//! Deliberately free of the server: no JSON-RPC, no `Server`, no turn plumbing. The app server
//! wires these into a turn; this crate stays testable on its own.

pub mod observe;
pub mod paths;
pub mod plan;
pub mod verify;

#[cfg(test)]
#[path = "observe_tests.rs"]
mod observe_tests;

#[cfg(test)]
#[path = "verify_tests.rs"]
mod verify_tests;

#[cfg(test)]
mod plan_tests;
