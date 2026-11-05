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
use joyczl_connector::{config, email, Outgoing};

pub async fn run(home: &Path, args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("list") | None => list(home),
        Some("email") => email_send(home, &args[1..]).await,
        Some(other) => bail!("不认识的连接器：{other}（有：email）"),
    }
}

fn list(home: &Path) -> Result<()> {
    let config = config::load(home)?;
    match config.email {
        None => println!(
            "还没有连接器。写 {} 就可以启用 email（白名单留空 = 谁都不能发）。",
            home.join("connectors.json").display()
        ),
        Some(email) => {
            println!(
                "email —— {}:{}，发件人 {}",
                email.smtp_host, email.smtp_port, email.from
            );
            println!(
                "  模式：{}",
                if email.live {
                    "live（模型每发一封都会问；从终端直接发不问）"
                } else {
                    "草稿（只写 outbox）"
                }
            );
            if email.allow_recipients.is_empty() {
                println!("  白名单：空 —— 谁都不能发");
            } else {
                println!("  白名单：{}", email.allow_recipients.join(", "));
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
            other => bail!("不认识的参数：{other}"),
        }
        index += 2;
    }
    let (Some(to), Some(subject), Some(body)) = (to, subject, body) else {
        bail!("用法：joy connector email --to 收件人 --subject 主题 --body 正文 [--dry-run]");
    };

    let config = config::load(home)?;
    let Some(email_config) = config.email else {
        bail!(
            "没配置 email 连接器：先写 {}",
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
            "草稿：{}（{}\n  要真发就在 connectors.json 里设 live 并去掉 --dry-run）",
            path.display(),
            if email_config.live {
                "这次的 --dry-run 要求"
            } else {
                "还没开 live"
            }
        );
        return Ok(());
    }
    let password = email::password_of(&email_config, home)?;
    email::send_live(&email_config, password, &message).await?;
    println!("已发给 {}。", recipients.join(", "));
    Ok(())
}
