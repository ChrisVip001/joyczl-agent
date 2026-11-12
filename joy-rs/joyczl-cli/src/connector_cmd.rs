//! `joy connector` — the human's own path to a connector.
//!
//! The tool (`email_send`) is how the **model** acts; this is how a person acts. Both go
//! through the same allowlist and the same wire code, so "it works from the terminal"
//! and "it works from a turn" cannot drift apart. The approval card is skipped here for
//! the obvious reason: the person typing the command **is** the approval.
//!
//!   joy connector list
//!   joy connector email --to a@b.c --subject 主题 --body 正文 [--dry-run]

use std::path::Path;

use anyhow::{bail, Result};
use joyczl_connector::{config, email, feishu, feishu_oauth, Outgoing};

pub async fn run(home: &Path, args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("list") | None => list(home),
        Some("email") => email_send(home, &args[1..]).await,
        Some("feishu") => match args.get(1).map(String::as_str) {
            Some("login") => feishu_login(home).await,
            Some("logout") => feishu_logout(home),
            Some("status") => feishu_status(home),
            _ => feishu_send(home, &args[1..]).await,
        },
        Some(other) => bail!("Unknown connector: {other} (known: email, feishu)"),
    }
}

fn list(home: &Path) -> Result<()> {
    let config = config::load(home)?;
    if let Some(feishu) = &config.feishu {
        println!(
            "feishu — app {}, receive_id_type={}",
            feishu.app_id, feishu.receive_id_type
        );
        println!(
            "  mode: {}",
            if feishu.live {
                "live (the model asks on every send; sending straight from the terminal does not ask)"
            } else {
                "draft (writes to outbox only)"
            }
        );
        if feishu.allow_recipients.is_empty() {
            println!("  allowlist: empty — nobody can be written to");
        } else {
            println!("  allowlist: {}", feishu.allow_recipients.join(", "));
        }
    }
    if config.email.is_none() && config.feishu.is_none() {
        println!(
            "No connectors yet. Write {} to enable email / feishu (an empty allowlist means nobody can be written to).",
            home.join("connectors.json").display()
        );
        return Ok(());
    }
    match config.email {
        None => {}
        Some(email) => {
            println!(
                "email — {}:{}, from {}",
                email.smtp_host, email.smtp_port, email.from
            );
            println!(
                "  mode: {}",
                if email.live {
                    "live (the model asks on every message; sending straight from the terminal does not ask)"
                } else {
                    "draft (writes to outbox only)"
                }
            );
            if email.allow_recipients.is_empty() {
                println!("  allowlist: empty — nobody can be written to");
            } else {
                println!("  allowlist: {}", email.allow_recipients.join(", "));
            }
        }
    }
    Ok(())
}

async fn email_send(home: &Path, args: &[String]) -> Result<()> {
    let mut to: Option<String> = None;
    let mut subject: Option<String> = None;
    let mut body: Option<String> = None;
    let mut dry_run = false;
    let mut index = 0;
    while index < args.len() {
        let value = args.get(index + 1).cloned();
        match args[index].as_str() {
            "--to" => to = value,
            "--subject" => subject = value,
            "--body" => body = value,
            "--dry-run" => {
                dry_run = true;
                index += 1;
                continue;
            }
            other => bail!("Unknown argument: {other}"),
        }
        index += 2;
    }
    let (Some(to), Some(subject), Some(body)) = (to, subject, body) else {
        bail!(
            "Usage: joy connector email --to recipient --subject subject --body body [--dry-run]"
        );
    };

    let config = config::load(home)?;
    let Some(email_config) = config.email else {
        bail!(
            "No email connector configured: write {} first",
            home.join("connectors.json").display()
        );
    };
    let recipients: Vec<String> = to
        .split([',', ';'])
        .map(str::trim)
        .filter(|addr| !addr.is_empty())
        .map(str::to_string)
        .collect();
    email::check_recipients(&email_config, &recipients, home)?;

    let message = Outgoing {
        from: email_config.from.clone(),
        to: recipients.clone(),
        subject,
        body,
    };

    if dry_run || !email_config.live {
        let path = email::write_draft(home, &message).await?;
        println!(
            "Draft: {} ({}\n  to actually send it, set live in connectors.json and drop --dry-run)",
            path.display(),
            if email_config.live {
                "--dry-run asked for this one"
            } else {
                "live is not on yet"
            }
        );
        return Ok(());
    }
    let password = email::password_of(&email_config, home)?;
    email::send_live(&email_config, password, &message).await?;
    println!("Sent to {}.", recipients.join(", "));
    Ok(())
}

/// `joy connector feishu login` — browser round trip, token to `<home>/connector-auth/`.
async fn feishu_login(home: &Path) -> Result<()> {
    let config = config::load(home)?;
    let Some(feishu_config) = config.feishu else {
        bail!(
            "No feishu connector configured: write {} first",
            home.join("connectors.json").display()
        );
    };
    let expires_at = feishu_oauth::login(&feishu_config, home).await?;
    println!(
        "Signed in as the Feishu user identity; the token expires at {} (file {}).\n         Set feishu.as to \"user\" in connectors.json to send as this identity.",
        feishu_oauth::expiry_text(expires_at),
        home.join("connector-auth/feishu.json").display()
    );
    Ok(())
}

fn feishu_logout(home: &Path) -> Result<()> {
    if feishu_oauth::logout(home)? {
        println!("Deleted the Feishu user token. To send as the user again, sign in again.");
    } else {
        println!("There was no session to begin with.");
    }
    Ok(())
}

fn feishu_status(home: &Path) -> Result<()> {
    let config = config::load(home)?;
    let identity = config
        .feishu
        .as_ref()
        .and_then(|feishu| feishu.identity().ok())
        .unwrap_or("app");
    println!("Sending identity: {identity}");
    match feishu_oauth::status(home) {
        None => println!("User token: none (sign in before sending as the user)"),
        Some((expired, expires_at)) => println!(
            "User token: {} (until {})",
            if expired {
                "expired; the next send will try to refresh it"
            } else {
                "valid"
            },
            feishu_oauth::expiry_text(expires_at)
        ),
    }
    Ok(())
}

async fn feishu_send(home: &Path, args: &[String]) -> Result<()> {
    let mut to: Option<String> = None;
    let mut text: Option<String> = None;
    let mut dry_run = false;
    let mut index = 0;
    while index < args.len() {
        let value = args.get(index + 1).cloned();
        match args[index].as_str() {
            "--to" => to = value,
            "--text" => text = value,
            "--dry-run" => {
                dry_run = true;
                index += 1;
                continue;
            }
            other => bail!("Unknown argument: {other}"),
        }
        index += 2;
    }
    let (Some(to), Some(text)) = (to, text) else {
        bail!("Usage: joy connector feishu --to recipient-id --text body [--dry-run]");
    };

    let config = config::load(home)?;
    let Some(feishu_config) = config.feishu else {
        bail!(
            "No feishu connector configured: write {} first",
            home.join("connectors.json").display()
        );
    };
    if !feishu_config.permits(&to) {
        bail!(
            "Recipient {to} is not on the allowlist. The allowlist is allow_recipients in {} (feishu section).",
            home.join("connectors.json").display()
        );
    }

    if dry_run || !feishu_config.live {
        let path = feishu::write_draft(home, &feishu_config, &to, &text).await?;
        println!(
            "Draft: {} ({}\n  to actually send it, set live in connectors.json and drop --dry-run)",
            path.display(),
            if feishu_config.live {
                "--dry-run asked for this one"
            } else {
                "live is not on yet"
            }
        );
        return Ok(());
    }
    // The CLI is the human acting directly: no approval card. The tool path still asks.
    let message_id = feishu::send_live(&feishu_config, home, &to, &text).await?;
    println!("Sent to {to} (message_id {message_id}).");
    Ok(())
}
