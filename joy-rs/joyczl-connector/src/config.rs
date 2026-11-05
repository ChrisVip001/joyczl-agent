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
