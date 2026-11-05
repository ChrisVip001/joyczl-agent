//! Connectors — the agent acting **in the world**, one human nod at a time.
//!
//! The local-first rule so far has been "draft, never send" (`send_message` → outbox).
//! A connector is the exception, and it is fenced accordingly:
//!
//!   1. **Off until configured.** No `<home>/connectors.json` → no connector tools exist;
//!      the model cannot even see their names.
//!   2. **Dry run by default.** A configured connector still writes drafts to
//!      `<home>/outbox/` unless the config says `"live": true`.
//!   3. **Recipient allowlist is a hard boundary.** Empty list = nobody. The model
//!      cannot widen it; only the human edits the file.
//!   4. **Every live action asks.** The approval card names the recipients and a body
//!      preview — a card that doesn't say what will happen is a card nobody can judge.

pub mod config;
pub mod email;
pub mod smtp;

pub use config::{ConnectorConfig, EmailConfig, RecipientPolicy};
pub use email::email_send_tool;

#[cfg(test)]
#[path = "email_tests.rs"]
mod email_tests;

/// A message that may leave the machine. Built by the tool, sent by a transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub from: String,
    pub to: Vec<String>,
    pub subject: String,
    pub body: String,
}
