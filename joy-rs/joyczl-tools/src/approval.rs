//! Interactive approval: **off by default, deny by default, silence means denial**.
//!
//! This layer has only one trait — the tool layer knows no protocol and no Server;
//! who asks and how is up to the implementer (same discipline as `subagent.rs`).
//! Implementations live in `joyczl-app-server`: it has the outlet and the wait table.
//!
//! Three rules that must not bend:
//!
//! 1. **Deny by default**. No broker (terminal not asking, cockpit not open), timeout,
//!    dropped connection, late answer — nothing passes; approval has one source: a "yes".
//! 2. **Approval only unlocks the allow table**. Hard-deny list and sandbox are non-negotiable:
//!    the gates in `vet` return before approval ever runs; approval never touches them.
//! 3. **Timeouts are the implementer's responsibility**. `request` must honor `timeout_secs`
//!    itself and return `false` — a request that never returns hangs a conversation forever.

use std::future::Future;
use std::pin::Pin;

/// A single approval request.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    /// What to run (tool name).
    pub tool: String,
    /// Human-facing preview: the raw command, or a summary of arguments.
    pub args_preview: String,
    /// Why it wasn't let through by the allow rules directly.
    pub reason: String,
    /// Seconds to wait for a human answer. The implementer must give up after it and return `false`.
    pub timeout_secs: i64,
}

pub type ApprovalFut = Pin<Box<dyn Future<Output = bool> + Send>>;

/// Who answers "should this run". The implementer handles asking, waiting, and timeouts.
pub trait ApprovalBroker: Send + Sync {
    /// `true` = approved; `false` = denied (including timeout, no answer, errors).
    fn request(&self, request: ApprovalRequest) -> ApprovalFut;
}
