//! `<home>/connectors.json` — intent, not secrets. The file says *which* connector and
//! where its password comes from; the password itself lives in the `SecretStore`
//! (keychain first, plaintext fallback, and the caller is told which).
//!
//! ```json
//! {
//!   "email": {
//!     "smtp_host": "smtp.example.com",
//!     "smtp_port": 465,
//!     "from": "joy@example.com",
//!     "username": "joy@example.com",
//!     "secret": "keychain:connector:email",
//!     "allow_recipients": ["@example.com", "alice@other.org"],
//!     "live": false
//!   }
//! }
//! ```
//!
//! `secret` takes `env:VAR` or anything else → `SecretStore` lookup under that account
//! name. `live: false` (the default) means drafts only; turning it on is a human act.

use std::path::Path;

use joyczl_config::models::SecretStore;
use serde::Deserialize;

/// The whole file. Unknown keys are rejected: a typo'd `allow_recipents` silently
/// meaning "allow nobody" is exactly the kind of quiet failure this repo refuses.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorConfig {
    #[serde(default)]
    pub email: Option<EmailConfig>,
    #[serde(default)]
    pub feishu: Option<FeishuConfig>,
}

/// 飞书（Lark）自建应用。消息以**应用身份**发出（tenant_access_token），
/// 这也是机器人能做的事；以用户身份行事需要 OAuth 授权码 + 控制台登记回调，
/// 见 docs/limitations：没有可验证的 app 之前不写不可验证的流程。
///
/// ```json
/// {
///   "feishu": {
///     "app_id": "cli_a…",
///     "app_secret": "keychain:connector:feishu",
///     "allow_recipients": ["oc_聊天id", "ou_用户id"],
///     "live": false
///   }
/// }
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeishuConfig {
    pub app_id: String,
    /// `env:VAR` or a `SecretStore` account name.
    pub app_secret: String,
    /// Who the message comes from: `app` (default, tenant token) or `user` (the person who
    /// ran `joy connector feishu login`). The human's decision, never guessed.
    #[serde(default, rename = "as")]
    pub identity: Option<String>,
    /// `open_id` (a person) or `chat_id` (a group) — must match the ids in the allowlist.
    #[serde(default = "default_receive_id_type")]
    pub receive_id_type: String,
    /// Hard boundary, same rule as email: empty = nobody.
    #[serde(default)]
    pub allow_recipients: Vec<String>,
    /// `false` (default): drafts to `<home>/outbox/`.
    #[serde(default)]
    pub live: bool,
    /// Override for tests and self-hosted Lark deployments; defaults to the public cloud.
    #[serde(default = "default_feishu_base")]
    pub base_url: String,
}

fn default_receive_id_type() -> String {
    "open_id".to_string()
}

fn default_feishu_base() -> String {
    "https://open.feishu.cn".to_string()
}

impl FeishuConfig {
    /// `app` (default) or `user` — validated at load so a typo cannot silently send as
    /// the wrong identity.
    pub fn identity(&self) -> Result<&str, String> {
        match self.identity.as_deref().unwrap_or("app") {
            identity @ ("app" | "user") => Ok(identity),
            other => Err(format!(
                "feishu.as must be app or user, got {other:?} (empty = app)"
            )),
        }
    }

    pub fn permits(&self, receive_id: &str) -> bool {
        let target = receive_id.trim().to_ascii_lowercase();
        !target.is_empty()
            && self
                .allow_recipients
                .iter()
                .any(|entry| entry.trim().to_ascii_lowercase() == target)
    }

    /// Resolve the app secret (keychain / env) — same rule as email's SMTP password.
    pub fn secret(&self, home: &Path) -> Option<String> {
        match self.app_secret.strip_prefix("env:") {
            Some(var) => std::env::var(var).ok().filter(|v| !v.trim().is_empty()),
            None => SecretStore::auto().get(home, &self.app_secret),
        }
    }
}

/// One email account. `smtp_port` decides the transport:
/// 465 = implicit TLS, 587 = STARTTLS, anything else = plaintext (dev servers only).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmailConfig {
    pub smtp_host: String,
    pub smtp_port: u16,
    pub from: String,
    /// SMTP AUTH user. `None` = no AUTH at all (a local relay, or a dev fake server).
    #[serde(default)]
    pub username: Option<String>,
    /// `env:VAR` or a `SecretStore` account name. Only needed when `username` is set.
    #[serde(default)]
    pub secret: Option<String>,
    /// Hard boundary. An entry is either an exact address (`a@b.c`) or a domain
    /// (`@b.c`). **Empty means nobody** — failing closed is the only honest default.
    #[serde(default)]
    pub allow_recipients: Vec<String>,
    /// `false` (default): everything goes to `<home>/outbox/`. `true`: real SMTP.
    #[serde(default)]
    pub live: bool,
    /// Inbound (0.13.0): set this block to poll the mailbox. Unset = no inbound at all,
    /// and no inbox surface anywhere.
    #[serde(default)]
    pub imap: Option<ImapSection>,
}

/// Mailbox polling. `mode` is the human's standing decision:
///   notify (default) — record + notify, a person decides what to do
///   turn             — new mail automatically becomes a turn (the model starts working
///                       on the standing authorization the human gave in this file)
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImapSection {
    pub host: String,
    /// 993 = implicit TLS, 143 = STARTTLS, anything else = plaintext (local test relay).
    #[serde(default = "default_imap_port")]
    pub port: u16,
    #[serde(default = "default_mailbox")]
    pub mailbox: String,
    /// Poll interval; 30s floor — polling faster than that is just hurting yourself.
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
    #[serde(default = "default_inbound_mode")]
    pub mode: String,
    /// Default to the email section's username / secret.
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub secret: Option<String>,
}

fn default_imap_port() -> u16 {
    993
}

fn default_mailbox() -> String {
    "INBOX".to_string()
}

fn default_poll_secs() -> u64 {
    300
}

fn default_inbound_mode() -> String {
    "notify".to_string()
}

impl ImapSection {
    /// Validate at load time: a typo in `mode` must fail the start, not silently mean
    /// "notify" while the human believes the model is handling mail.
    pub fn validate(&self) -> Result<(), String> {
        if self.mode != "notify" && self.mode != "turn" {
            return Err(format!(
                "imap.mode must be notify or turn, got {:?}",
                self.mode
            ));
        }
        if self.poll_secs < 30 {
            return Err(format!(
                "imap.poll_secs must not be under 30 (got {})",
                self.poll_secs
            ));
        }
        Ok(())
    }

    pub fn username(&self, email: &EmailConfig) -> Option<String> {
        self.username.clone().or_else(|| email.username.clone())
    }

    pub fn secret(&self, email: &EmailConfig) -> Option<String> {
        self.secret.clone().or_else(|| email.secret.clone())
    }
}

impl EmailConfig {
    /// Is this recipient allowed by the configured policy?
    pub fn permits(&self, to: &str) -> bool {
        let policy = RecipientPolicy {
            entries: self.allow_recipients.clone(),
        };
        policy.permits(to)
    }

    /// Resolve the SMTP password, if one is configured.
    pub fn password(&mut self, home: &Path) -> Option<String> {
        let source = self.secret.clone()?;
        match source.strip_prefix("env:") {
            Some(var) => std::env::var(var).ok().filter(|v| !v.trim().is_empty()),
            None => SecretStore::auto().get(home, &source),
        }
    }
}

/// Allowlist matching: exact address, or `@domain` for the whole domain.
#[derive(Debug, Clone)]
pub struct RecipientPolicy {
    pub entries: Vec<String>,
}

impl RecipientPolicy {
    pub fn permits(&self, to: &str) -> bool {
        let to = to.trim().to_ascii_lowercase();
        if to.is_empty() || !to.contains('@') {
            return false;
        }
        self.entries.iter().any(|entry| {
            let entry = entry.trim().to_ascii_lowercase();
            if entry.is_empty() {
                false
            } else if let Some(domain) = entry.strip_prefix('@') {
                to.rsplit_once('@').is_some_and(|(_, d)| d == domain)
            } else {
                entry == to
            }
        })
    }
}

/// Load the file. Missing file = no connectors (not an error); a malformed file is.
pub fn load(home: &Path) -> anyhow::Result<ConnectorConfig> {
    let config = load_raw(home)?;
    if let Some(feishu) = &config.feishu {
        feishu
            .identity()
            .map_err(|e| anyhow::anyhow!("feishu in connectors.json has a problem: {e}"))?;
    }
    if let Some(email) = &config.email {
        if let Some(imap) = &email.imap {
            imap.validate()
                .map_err(|e| anyhow::anyhow!("email.imap in connectors.json has a problem: {e}"))?;
        }
    }
    Ok(config)
}

fn load_raw(home: &Path) -> anyhow::Result<ConnectorConfig> {
    let path = home.join("connectors.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Ok(ConnectorConfig::default());
    };
    serde_json::from_str(&raw).map_err(|e| {
        anyhow::anyhow!(
            "{} could not be read: {e}\n\n             connector config is hand-written; a mistake leaves nobody able to use it — better to fail loudly than to guess.",
            path.display()
        )
    })
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
