//! The server side of the approval question.
//!
//! `joyczl-tools` only knows the `ApprovalBroker` trait (the tool layer knows neither protocol nor
//! Server); this is its implementation: emit one `approvalRequested` notification, register the answer
//! channel into the waiting table, then **wait**. No answer (timeout, lost connection, too late) means rejection — default-deny is the foundation.
//!
//! The waiting table is bucketed by turn_id, the same shape as `turns` (the cancellation-token table):
//! a finished turn removes the whole bucket, so there is never "last turn's question still waiting for an answer that will never come".

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use joyczl_protocol::{ApprovalRequestedNotification, ServerNotification};
use joyczl_tools::approval::{ApprovalBroker, ApprovalFut, ApprovalRequest, RememberTarget};
use tokio::sync::oneshot;

use crate::{narrow, EventSink};

/// One question currently waiting for a human answer.
pub(crate) struct Waiting {
    /// The answer comes back here.
    pub(crate) tx: oneshot::Sender<bool>,
    /// What "记住这条" would record, as named by the asking tool (the command, or the directory).
    ///
    /// Kept **here**, not inferred from the preview text at answer time: parsing a human-facing
    /// string to decide what gets a standing permission is how the wrong thing gets remembered.
    pub(crate) remember: Option<RememberTarget>,
}

/// turn_id → request_id → the waiting question.
pub(crate) type Pending = Arc<Mutex<HashMap<String, HashMap<String, Waiting>>>>;

pub(crate) struct Bridge {
    pub(crate) pending: Pending,
    pub(crate) sink: EventSink,
    pub(crate) turn_id: String,
    pub(crate) timeout_secs: i64,
    /// People off-screen should also know "approval is being awaited" — a side path; absent means nothing is sent.
    pub(crate) notify: Option<Arc<dyn joyczl_tools::notify::Notifier>>,
    /// Audit ledger (M3b): all four outcome states each leave one row.
    pub(crate) logbook: joyczl_state::Approvals,
}

impl ApprovalBroker for Bridge {
    fn request(&self, request: ApprovalRequest) -> ApprovalFut {
        let request_id = format!("ap-{}", next_request_id());
        let (tx, rx) = oneshot::channel();

        if let Ok(mut table) = self.pending.lock() {
            table.entry(self.turn_id.clone()).or_default().insert(
                request_id.clone(),
                Waiting {
                    tx,
                    remember: request.remember.clone(),
                },
            );
        }

        // The ledger still needs these three values after they are moved into the notification — keep copies first.
        let tool = request.tool.clone();
        let args_preview = request.args_preview.clone();
        let reason = request.reason.clone();

        if let Some(notifier) = &self.notify {
            notifier.notify(joyczl_tools::notify::NotifyEvent::ApprovalRequested {
                turn_id: self.turn_id.clone(),
                tool: request.tool.clone(),
                args_preview: request.args_preview.clone(),
                reason: request.reason.clone(),
            });
        }

        self.sink
            .notification(ServerNotification::ApprovalRequested(
                ApprovalRequestedNotification {
                    turn_id: self.turn_id.clone(),
                    request_id: request_id.clone(),
                    tool: request.tool,
                    args_preview: request.args_preview,
                    reason: request.reason,
                    expires_in_ms: narrow(self.timeout_secs.saturating_mul(1000)),
                },
            ));

        let pending = self.pending.clone();
        let turn_id = self.turn_id.clone();
        let logbook = self.logbook.clone();
        let timeout = Duration::from_secs(self.timeout_secs.max(1) as u64);

        Box::pin(async move {
            // Timeout counts as rejection. A timeout here is **mandatory**: an approval request that waits
            // forever would hang a whole conversation permanently, and the tool layer's timeout cannot cover
            // "the server is waiting". The four-state outcome is also logged to the audit ledger (M3b):
            // a write failure does not block the main path — the ledger is a shadow; causality is the oneshot's.
            let outcome = match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(true)) => "approved",
                Ok(Ok(false)) => "denied",
                Ok(Err(_)) => "voided", // Sender dropped = voided because the turn ended
                Err(_) => "timeout",
            };
            if let Err(e) = logbook
                .log(
                    &turn_id,
                    &request_id,
                    &tool,
                    &args_preview,
                    &reason,
                    outcome,
                )
                .await
            {
                eprintln!("(joy) the approval audit record was not written ({outcome}): {e}");
            }
            let approved = outcome == "approved";
            // Deregister no matter the outcome — otherwise the table grows forever.
            if let Ok(mut table) = pending.lock() {
                if let Some(per_turn) = table.get_mut(&turn_id) {
                    per_turn.remove(&request_id);
                    if per_turn.is_empty() {
                        table.remove(&turn_id);
                    }
                }
            }
            if !approved {
                eprintln!("(joy) approval request {request_id} was not approved in time; treating it as a denial");
            }
            approved
        })
    }
}

/// Request number: in-process monotonicity is enough (a turn has few questions anyway).
fn next_request_id() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}
