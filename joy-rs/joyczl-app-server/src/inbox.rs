//! The connector inbox, inbound half: polling, recording, and handing mail to a turn.
//!
//! Two ways mail becomes work, both of them a human's decision:
//!   - `inbound.mode = "notify"` (default): record + knock; the panel's button
//!     (`inbox/summarize`) is the human saying "handle these now".
//!   - `inbound.mode = "turn"`: the same hand-off happens on its own — a standing
//!     authorization written in connectors.json by the human. The model never decides
//!     this for itself; it is config, and config is the human's.
//!
//! Fetching never marks mail as read (`BODY.PEEK[]`): Joy reading a mailbox must not
//! change what another person's mail client shows.

use std::time::Duration;

use joyczl_protocol::ErrorObject;
use joyczl_protocol::{
    codes, InboxItemView, InboxListView, InboxSummarizeParams, InboxSummarizeResponse, RequestId,
    ServerNotification, TurnStartParams,
};

use crate::{EventSink, Server};

/// The session auto-handled mail lands in, unless the caller names another.
pub(crate) const INBOX_SESSION: &str = "inbox";
/// How many messages one hand-off takes. More than this and the turn becomes a digest
/// of a backlog instead of a summary of what just arrived.
const HANDOFF_LIMIT: i64 = 10;
/// How much of each body goes into the turn's message.
const BODY_PREVIEW_CHARS: usize = 1500;
/// How many items the panel lists.
const LIST_LIMIT: i64 = 20;

/// Start the poller. Cheap and silent when no `imap` block exists: no ticker, no noise.
pub(crate) fn spawn(server: &Server, sink: &EventSink) {
    let server = server.clone();
    let sink = sink.clone();
    tokio::spawn(async move { run(&server, &sink).await });
}

async fn run(server: &Server, sink: &EventSink) {
    let home = server.home();
    let config = match joyczl_connector::config::load(&home) {
        Ok(config) => config,
        Err(e) => {
            eprintln!(
                "(joy) connector config could not be read; inbound polling did not start: {e}"
            );
            return;
        }
    };
    let Some(email) = config.email else {
        return;
    };
    let Some(imap) = email.imap.clone() else {
        return;
    };
    let period = Duration::from_secs(imap.poll_secs);
    eprintln!(
        "(joy) inbox polling enabled: {}:{} {} every {}s (mode={})",
        imap.host, imap.port, imap.mailbox, imap.poll_secs, imap.mode
    );

    loop {
        match poll_once(server, sink, &email, &imap).await {
            Ok(0) => {}
            Ok(added) => eprintln!("(joy) {added} new messages in the inbox"),
            // Never silent, never fatal: the next tick is the retry.
            Err(e) => eprintln!(
                "(joy) inbox polling failed (retrying in {}s): {e}",
                imap.poll_secs
            ),
        }
        tokio::time::sleep(period).await;
    }
}

async fn poll_once(
    server: &Server,
    sink: &EventSink,
    email: &joyczl_connector::EmailConfig,
    imap: &joyczl_connector::ImapSection,
) -> anyhow::Result<usize> {
    let Some(username) = imap.username(email) else {
        anyhow::bail!("imap has no username, and no email.username to fall back on");
    };
    let password = match imap.secret(email) {
        Some(source) => {
            let resolved = match source.strip_prefix("env:") {
                Some(var) => std::env::var(var).ok(),
                None => joyczl_config::models::SecretStore::auto().get(&server.home(), &source),
            };
            resolved.filter(|value| !value.trim().is_empty())
        }
        None => None,
    };
    // A local relay may allow anonymous LOGIN; otherwise a missing secret is a config bug,
    // said once per tick rather than swallowed.
    let Some(password) = password else {
        anyhow::bail!(
            "the imap password is missing (the entry the secret points at does not exist)"
        );
    };

    let config = joyczl_connector::ImapConfig {
        host: imap.host.clone(),
        port: imap.port,
        username,
        password,
        mailbox: imap.mailbox.clone(),
    };
    let cursor = server.inbox.cursor("email", &imap.mailbox).await?;
    let outcome = joyczl_connector::imap::fetch(&config, cursor).await?;

    // Advance the cursor even when nothing was added: a message we failed to record must
    // not block the ones behind it forever.
    if let Some(high_water) = outcome.server_max_uid {
        server
            .inbox
            .set_cursor("email", &imap.mailbox, high_water)
            .await?;
    }

    let mut added = 0;
    for message in &outcome.messages {
        // A Date header is optional; "when we saw it" is a fine second best.
        let received_at = if message.date.trim().is_empty() {
            chrono::Utc::now().to_rfc3339()
        } else {
            message.date.clone()
        };
        let record = server
            .inbox
            .record(
                "email",
                &imap.mailbox,
                message.uid,
                message.message_id.as_deref(),
                &message.from,
                &message.subject,
                &received_at,
                &message.body,
            )
            .await?;
        added += usize::from(record);
    }

    if added > 0 {
        sink.notification(ServerNotification::InboxReceived(
            joyczl_protocol::InboxReceivedNotification {
                connector: "email".to_string(),
                added: added as i32,
            },
        ));
        if imap.mode == "turn" {
            // The standing authorization: same path as the button, just not waiting for it.
            match hand_off(server, sink, INBOX_SESSION).await {
                Ok(response) => eprintln!(
                    "(joy) inbound auto-handling: {} ({} messages)",
                    response.message, response.count
                ),
                Err(e) => eprintln!("(joy) inbound auto-handling did not start: {}", e.message),
            }
        }
    }
    Ok(added)
}

/// `inbox/list` — the panel's data.
pub(crate) async fn list(server: &Server) -> Result<InboxListView, ErrorObject> {
    let items = server
        .inbox
        .recent("email", LIST_LIMIT)
        .await
        .map_err(internal)?;
    let unseen = server.inbox.unseen_count("email").await.map_err(internal)?;
    Ok(InboxListView {
        items: items
            .into_iter()
            .map(|item| InboxItemView {
                id: item.id,
                from_addr: item.from_addr,
                subject: item.subject,
                received_at: item.received_at,
                snippet: item.snippet,
                seen: item.seen,
            })
            .collect(),
        unseen,
    })
}

/// `inbox/summarize` — the human's "handle these": one turn over the unseen mail.
pub(crate) async fn summarize(
    server: &Server,
    sink: &EventSink,
    params: InboxSummarizeParams,
) -> Result<InboxSummarizeResponse, ErrorObject> {
    let session = params
        .session_id
        .unwrap_or_else(|| INBOX_SESSION.to_string());
    hand_off(server, sink, &session).await
}

/// Shared by the button and the standing `turn` policy. Marks mail handled **only after**
/// the turn is actually started — a failed start must leave the mail waiting, not lost.
async fn hand_off(
    server: &Server,
    sink: &EventSink,
    session: &str,
) -> Result<InboxSummarizeResponse, ErrorObject> {
    let items = server
        .inbox
        .unseen("email", HANDOFF_LIMIT)
        .await
        .map_err(internal)?;
    if items.is_empty() {
        return Ok(InboxSummarizeResponse {
            count: 0,
            session_id: session.to_string(),
            message: "Nothing in the inbox is waiting for you.".to_string(),
        });
    }

    // No model, no hand-off — and no mail marked handled either.
    if server.resolved().is_none() {
        return Err(ErrorObject {
            code: codes::PROVIDER_ERROR,
            message: "No model is configured, so the mail cannot be handled; the messages stay in the inbox as they are.".to_string(),
            data: None,
        });
    }

    let mut message = format!(
        "{} new messages arrived in the inbox. Summarise each one and flag the ones that need a reply (read-only — do not send anything on my behalf):\n",
        items.len()
    );
    for (index, item) in items.iter().enumerate() {
        message.push_str(&format!(
            "\n{}. From: {} | Subject: {} | Time: {}\n{}\n",
            index + 1,
            item.from_addr,
            item.subject,
            item.received_at,
            item.body
                .chars()
                .take(BODY_PREVIEW_CHARS)
                .collect::<String>()
        ));
    }

    let ids: Vec<i64> = items.iter().map(|item| item.id).collect();
    let count = ids.len() as i64;
    server.inbox.mark_handled(&ids).await.map_err(internal)?;

    let session = session.to_string();
    let server = server.clone();
    let sink = sink.clone();
    let spawn_session = session.clone();
    tokio::spawn(async move {
        let params = TurnStartParams {
            attachments: None,
            session_id: Some(spawn_session),
            message,
            stream: Some(true),
            cwd: None,
            tier: None,
        };
        if let Err(e) = crate::turn::run_turn(&server, params, RequestId::Number(-1), &sink).await {
            // The mail is already marked handled; a failed turn must still be visible.
            eprintln!(
                "(joy) the turn that handles the mail did not start: {}",
                e.message
            );
        }
    });

    Ok(InboxSummarizeResponse {
        count,
        session_id: session,
        message: format!("{count} messages were handed to a turn."),
    })
}

fn internal(error: sqlx::Error) -> ErrorObject {
    ErrorObject {
        code: codes::INTERNAL_ERROR,
        message: format!("the inbox could not be read or written: {error}"),
        data: None,
    }
}
