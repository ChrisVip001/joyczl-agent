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
    let path = home.join("connectors.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Ok(ConnectorConfig::default());
    };
    serde_json::from_str(&raw).map_err(|e| {
        anyhow::anyhow!(
            "{} 读不明白：{e}\n\
             连接器配置是手写的，写错就没人能用 —— 这里宁可大声失败，也不猜。",
            path.display()
        )
    })
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
