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
        Some(other) => bail!("不认识的连接器：{other}（有：email、feishu）"),
    }
}

fn list(home: &Path) -> Result<()> {
    let config = config::load(home)?;
    if let Some(feishu) = &config.feishu {
        println!(
            "feishu —— 应用 {}，receive_id_type={}",
            feishu.app_id, feishu.receive_id_type
        );
        println!(
            "  模式：{}",
            if feishu.live {
                "live（模型每发一条都会问；从终端直接发不问）"
            } else {
                "草稿（只写 outbox）"
            }
        );
        if feishu.allow_recipients.is_empty() {
            println!("  白名单：空 —— 谁都不能发");
        } else {
            println!("  白名单：{}", feishu.allow_recipients.join(", "));
        }
    }
    if config.email.is_none() && config.feishu.is_none() {
        println!(
            "还没有连接器。写 {} 就可以启用 email / feishu（白名单留空 = 谁都不能发）。",
            home.join("connectors.json").display()
        );
        return Ok(());
    }
    match config.email {
        None => {}
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

/// `joy connector feishu login` — browser round trip, token to `<home>/connector-auth/`.
async fn feishu_login(home: &Path) -> Result<()> {
    let config = config::load(home)?;
    let Some(feishu_config) = config.feishu else {
        bail!(
            "没配置 feishu 连接器：先写 {}",
            home.join("connectors.json").display()
        );
    };
    let expires_at = feishu_oauth::login(&feishu_config, home).await?;
    println!(
        "已登录飞书用户身份，token 到 {} 过期（文件 {}）。\n\
         把 connectors.json 里 feishu.as 设成 \"user\" 才会以这个身份发消息。",
        feishu_oauth::expiry_text(expires_at),
        home.join("connector-auth/feishu.json").display()
    );
    Ok(())
}

fn feishu_logout(home: &Path) -> Result<()> {
    if feishu_oauth::logout(home)? {
        println!("已删掉飞书用户 token。之后要用用户身份发消息，得重新 login。");
    } else {
        println!("本来就没有登录。");
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
    println!("发送身份：{identity}");
    match feishu_oauth::status(home) {
        None => println!("用户 token：没有（以用户身份发消息前先 login）"),
        Some((expired, expires_at)) => println!(
            "用户 token：{}（到 {}）",
            if expired {
                "已过期，下次发送会尝试续期"
            } else {
                "有效"
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
            other => bail!("不认识的参数：{other}"),
        }
        index += 2;
    }
    let (Some(to), Some(text)) = (to, text) else {
        bail!("用法：joy connector feishu --to 接收方id --text 正文 [--dry-run]");
    };

    let config = config::load(home)?;
    let Some(feishu_config) = config.feishu else {
        bail!(
            "没配置 feishu 连接器：先写 {}",
            home.join("connectors.json").display()
        );
    };
    if !feishu_config.permits(&to) {
        bail!(
            "接收方 {to} 不在白名单里。白名单在 {} 的 allow_recipients（飞书段）。",
            home.join("connectors.json").display()
        );
    }

    if dry_run || !feishu_config.live {
        let path = feishu::write_draft(home, &feishu_config, &to, &text).await?;
        println!(
            "草稿：{}（{}
  要真发就在 connectors.json 里设 live 并去掉 --dry-run）",
            path.display(),
            if feishu_config.live {
                "这次的 --dry-run 要求"
            } else {
                "还没开 live"
            }
        );
        return Ok(());
    }
    // The CLI is the human acting directly: no approval card. The tool path still asks.
    let message_id = feishu::send_live(&feishu_config, home, &to, &text).await?;
    println!("已发给 {to}（message_id {message_id}）。");
    Ok(())
}
