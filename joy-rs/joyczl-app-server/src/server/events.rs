//! Events responsibilities of the app-server.

use joyczl_protocol::{JsonRpcMessage, ServerNotification};
use tokio::sync::mpsc;

/// One frame from server → client. Notifications and responses share the same channel — order is thereby guaranteed.
pub enum Frame {
    Notification(ServerNotification),
    Response(JsonRpcMessage),
}

/// The outlet for pushing things to the client. The turn execution holds it and sends text deltas
/// as soon as they arrive, instead of waiting for the whole turn to finish.
#[derive(Clone)]
pub struct EventSink {
    pub(crate) tx: mpsc::UnboundedSender<Frame>,
}

impl EventSink {
    pub(crate) fn new(tx: mpsc::UnboundedSender<Frame>) -> Self {
        Self { tx }
    }

    /// Outlet for in-process clients (terminal REPL): it starts its own channel, and notifications and
    /// responses are taken frame by frame from the receiver. The same order guarantee as stdio.
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<Frame>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }

    /// A send failure has only one possible cause: the writer is already gone (the process is exiting).
    /// Worrying about that notification then is pointless, so stay silent.
    pub(crate) fn notification(&self, notification: ServerNotification) {
        let _ = self.tx.send(Frame::Notification(notification));
    }

    pub(crate) fn response(&self, response: JsonRpcMessage) {
        let _ = self.tx.send(Frame::Response(response));
    }
}
