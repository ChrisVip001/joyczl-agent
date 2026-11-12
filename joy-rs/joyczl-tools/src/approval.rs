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
//!
//! The call site names **what** could be remembered ([`RememberTarget`]); how long a yes lasts
//! (this once / this process / forever) is the answerer's decision, so it stays out of this layer.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

/// What a "remember this" answer should write down.
///
/// Naming it here — rather than making the answerer parse the preview text — keeps "what the user
/// approved is exactly what is remembered". Nothing in here can relax a hard gate: a remembered
/// command still meets hard-deny and the sandbox, and a remembered directory still has to be inside
/// the writable boundary (`contained` runs first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RememberTarget {
    /// An exact command line (`run_command`). No wildcards: what the human saw was this line.
    Command(String),
    /// A directory (write tools). The file name is deliberately **not** remembered — the human
    /// approved "this folder", not "this one write".
    Directory(PathBuf),
}

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
    /// What "remember this" would record. `None` = nothing worth remembering (ask every time).
    pub remember: Option<RememberTarget>,
}

pub type ApprovalFut = Pin<Box<dyn Future<Output = bool> + Send>>;

/// Who answers "should this run". The implementer handles asking, waiting, and timeouts.
pub trait ApprovalBroker: Send + Sync {
    /// `true` = approved; `false` = denied (including timeout, no answer, errors).
    fn request(&self, request: ApprovalRequest) -> ApprovalFut;
}
