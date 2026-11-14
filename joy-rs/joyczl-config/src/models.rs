//! `<home>/models.json` — multiple "vendor + model + credential" profiles, switchable at will.
//!
//! Division of labor with `settings.json`: **that one holds runtime parameters** (rounds,
//! tokens, timeouts, gates), **this one holds which vendor's model, with which key** — it
//! must be UI-editable, credentials prefer the keychain: hence a separate file and methods.
//!
//! Three things are discipline in this implementation, not optional optimizations:
//!
//! * **Credentials never default to plaintext on disk**: macOS uses the Keychain (`security`),
//!   Linux uses `secret-tool`, Windows uses the Credential Manager (`keyring` crate); only if
//!   they all fail fall back to `<home>/secrets.json` (0600 on Unix).
//! * **Tests never touch the real keychain**: `SecretStore::memory()` / `file_only()`
//!   let unit tests run in a temp dir, without system auth prompts or keychain writes.
//! * **A profile is just "config"**: whether it applies to the current session is decided
//!   by `model/use`; editing the file does not affect the turn already running.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Service name in the keychain (shared by all three platforms).
pub const SERVICE: &str = "joyczl";

/// One model profile.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelProfile {
    /// Short name chosen by the user (`ds`, `company-openai`...); used when switching.
    pub id: String,
    /// Provider id of `joyczl-provider` (one of the 12, or `custom` with base_url).
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
    /// Cheap model: for the retrieval gate, consolidation, and the goal judge; provider default if absent.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub small_model: Option<String>,
    /// Self-hosted/proxied endpoint. Empty = the provider's own.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub base_url: Option<String>,
    /// Where credentials come from: `env:VAR` / `keychain` / `file`; `None` = this vendor
    /// needs no key (ollama, LM Studio). **Only a pointer is stored here, never the key itself.**
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub key_source: Option<String>,
    /// Whether **this profile's model** accepts images. Absent = the vendor's default
    /// (catalog row); `false` = text-only even on a vendor that ships vision models.
    /// Declared when the model is added, because "which model" and "what it can see" are
    /// the same decision — one vendor can ship both kinds.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub vision: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub capabilities: Option<joyczl_protocol::ModelCapabilities>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelsFile {
    /// id of the currently active profile.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub active: Option<String>,
    #[serde(default)]
    pub profiles: Vec<ModelProfile>,
}

pub fn models_path(home: &Path) -> PathBuf {
    home.join("models.json")
}

fn secrets_path(home: &Path) -> PathBuf {
    home.join("secrets.json")
}

pub fn load_models(home: &Path) -> ModelsFile {
    std::fs::read_to_string(models_path(home))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save_models(home: &Path, models: &ModelsFile) -> std::io::Result<()> {
    std::fs::write(models_path(home), serde_json::to_string_pretty(models)?)
}

pub fn find<'a>(models: &'a ModelsFile, id: &str) -> Option<&'a ModelProfile> {
    models.profiles.iter().find(|p| p.id == id)
}

pub fn active_profile(models: &ModelsFile) -> Option<&ModelProfile> {
    // If the id `active` points to was deleted, treat as absent — no dangling pointer.
    models.active.as_deref().and_then(|id| find(models, id))
}

/// Write (or overwrite by same id) one profile. **No key given keeps the original key_source** —
/// "switching" or "changing models" must not force the user to re-enter the key.
pub fn upsert(models: &mut ModelsFile, incoming: ModelProfile) {
    match models.profiles.iter_mut().find(|p| p.id == incoming.id) {
        Some(existing) => {
            let keep_key = incoming
                .key_source
                .clone()
                .or_else(|| existing.key_source.clone());
            *existing = ModelProfile {
                key_source: keep_key,
                ..incoming
            };
        }
        None => models.profiles.push(incoming),
    }
}

pub fn remove(models: &mut ModelsFile, id: &str) -> bool {
    let before = models.profiles.len();
    models.profiles.retain(|p| p.id != id);
    if models.active.as_deref() == Some(id) {
        models.active = None;
    }
    models.profiles.len() != before
}

/// Apply the active profile's **config** into Settings (credentials fetched separately, see
/// `apply_active`). Leaves `settings.json` alone: that file holds runtime parameters.
pub fn apply_active(
    models: &ModelsFile,
    home: &Path,
    store: &mut dyn SecretVault,
    settings: &mut crate::Settings,
) {
    let Some(profile) = active_profile(models) else {
        return;
    };
    apply_profile(profile, home, store, settings);
}

/// Apply **any** profile into Settings (used when routing picks models by tier — shares
/// the same key-fetching path as apply_active).
pub fn apply_profile(
    profile: &ModelProfile,
    home: &Path,
    store: &mut dyn SecretVault,
    settings: &mut crate::Settings,
) {
    settings.provider = profile.provider.clone();
    settings.model = profile.model.clone();
    settings.small_model = profile.small_model.clone();
    settings.base_url = profile.base_url.clone();
    settings.api_key = resolve_key(profile, home, store);
    // Per-model, not per-vendor: absent here means "fall back to the catalog row".
    settings.vision = profile.vision;
    settings.model_capabilities = profile.capabilities.clone();
}

/// Fetch the real key per `key_source`. `env:` reads the env var directly (**never copied
/// into files**); `keychain`/`file` go through `SecretStore`; absent = None (local models).
pub fn resolve_key(
    profile: &ModelProfile,
    home: &Path,
    store: &mut dyn SecretVault,
) -> Option<String> {
    let source = profile.key_source.as_deref()?;
    match source.strip_prefix("env:") {
        Some(var) => std::env::var(var).ok().filter(|v| !v.trim().is_empty()),
        None => store.get(home, &profile.id),
    }
}

/// Where credentials live. `Auto` is the default: use the system keychain when possible, else file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretBackend {
    Auto,
    File,
    /// Lives only in process memory: for tests, and for users explicitly choosing "no disk".
    Memory,
}

pub struct SecretStore {
    backend: SecretBackend,
    memory: HashMap<String, String>,
}

impl SecretStore {
    pub fn auto() -> Self {
        Self {
            backend: SecretBackend::Auto,
            memory: HashMap::new(),
        }
    }

    pub fn file_only() -> Self {
        Self {
            backend: SecretBackend::File,
            memory: HashMap::new(),
        }
    }

    pub fn memory() -> Self {
        Self {
            backend: SecretBackend::Memory,
            memory: HashMap::new(),
        }
    }

    pub fn backend(&self) -> SecretBackend {
        self.backend
    }

    /// Switch backend. **Only for tests and "never touch the keychain" scenarios**: the default
    /// is `Auto`; unit tests must pin `File` (temp dir) or `Memory` — no system auth prompts in CI.
    pub fn set_backend(&mut self, backend: SecretBackend) {
        self.backend = backend;
    }

    /// Store a key, returning **where it actually landed** (`keychain` / `file` / `memory`) —
    /// UI and logs must report the truth: never imply keychain when it is a plaintext file.
    pub fn put(
        &mut self,
        home: &Path,
        account: &str,
        secret: &str,
    ) -> Result<&'static str, String> {
        match self.backend {
            SecretBackend::Memory => {
                self.memory.insert(account.to_string(), secret.to_string());
                Ok("memory")
            }
            SecretBackend::File => write_file_secret(home, account, secret).map(|()| "file"),
            SecretBackend::Auto => {
                if os_keychain_put(account, secret) {
                    Ok("keychain")
                } else {
                    write_file_secret(home, account, secret).map(|()| "file")
                }
            }
        }
    }

    pub fn get(&mut self, home: &Path, account: &str) -> Option<String> {
        match self.backend {
            SecretBackend::Memory => self.memory.get(account).cloned(),
            SecretBackend::File => read_file_secret(home, account),
            SecretBackend::Auto => {
                os_keychain_get(account).or_else(|| read_file_secret(home, account))
            }
        }
    }

    pub fn remove(&mut self, home: &Path, account: &str) {
        match self.backend {
            SecretBackend::Memory => {
                self.memory.remove(account);
            }
            SecretBackend::File => {
                let _ = remove_file_secret(home, account);
            }
            SecretBackend::Auto => {
                os_keychain_remove(account);
                let _ = remove_file_secret(home, account);
            }
        }
    }
}

/// The one place secrets pass through: LLM keys, connector tokens and MCP tokens all come
/// through this seam, sooner or later.
///
/// Today's implementation is [`SecretStore`] (keychain / 0600 file / memory). The trait stays
/// for the **second backend**: when a server-shaped KMS envelope appears, only an
/// implementation is added and callers like `apply_profile` do not change a line. The
/// connector and MCP token files have not moved in yet — their rotation and locking rules
/// (0600 plus temp-file rename plus a cross-process lock) deserve a round of their own; the
/// time to move them is when the product actually ships a server edition.
pub trait SecretVault {
    /// Store a secret and return where it **actually landed** (`keychain` / `file` /
    /// `memory`) — the UI and the logs have to tell the truth: never imply "keychain" when
    /// it is really a plaintext file.
    fn put(&mut self, home: &Path, account: &str, secret: &str) -> Result<&'static str, String>;

    /// Read it back. `None` = never stored (a profile or service that needs no key at all).
    fn get(&mut self, home: &Path, account: &str) -> Option<String>;

    /// Delete it (the keychain entry and the file fallback are both cleared).
    fn remove(&mut self, home: &Path, account: &str);
}

impl SecretVault for SecretStore {
    fn put(&mut self, home: &Path, account: &str, secret: &str) -> Result<&'static str, String> {
        SecretStore::put(self, home, account, secret)
    }

    fn get(&mut self, home: &Path, account: &str) -> Option<String> {
        SecretStore::get(self, home, account)
    }

    fn remove(&mut self, home: &Path, account: &str) {
        SecretStore::remove(self, home, account);
    }
}

// ---- File fallback: `<home>/secrets.json`, 0600 ----------------------------------------------

fn read_secrets(home: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(secrets_path(home))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_secrets(home: &Path, secrets: &HashMap<String, String>) -> Result<(), String> {
    let raw = serde_json::to_string_pretty(secrets).map_err(|e| e.to_string())?;
    std::fs::write(secrets_path(home), raw).map_err(|e| e.to_string())?;
    // Plaintext fallback; tighten permissions when possible (Windows has no 0600 — in limitations).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ =
            std::fs::set_permissions(secrets_path(home), std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn write_file_secret(home: &Path, account: &str, secret: &str) -> Result<(), String> {
    let mut all = read_secrets(home);
    all.insert(account.to_string(), secret.to_string());
    write_secrets(home, &all)
}

fn read_file_secret(home: &Path, account: &str) -> Option<String> {
    read_secrets(home).get(account).cloned()
}

fn remove_file_secret(home: &Path, account: &str) -> Result<(), String> {
    let mut all = read_secrets(home);
    if all.remove(account).is_none() {
        return Ok(());
    }
    write_secrets(home, &all)
}

// ---- System keychain: macOS `security` / Linux `secret-tool` ---------------------------------

/// Run a keychain command, **with a timeout**.
///
/// `security` / `secret-tool` may pop dialogs or hang in unattended environments (CI,
/// remote sessions, ssh without GUI). Freezing the app-server on one credential operation
/// is unacceptable — timeout = failure, fall back to file. Non-empty stdout = real success.
fn run_limited(command: &mut std::process::Command, stdin_secret: Option<&str>) -> Option<String> {
    use std::io::Write;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let mut child = command
        .stdin(if stdin_secret.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let Some(secret) = stdin_secret {
        if let Some(mut pipe) = child.stdin.take() {
            let _ = pipe.write_all(secret.as_bytes());
        }
    }
    let deadline = Instant::now() + Duration::from_secs(KEYCHAIN_TIMEOUT_SECS);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let mut out = String::new();
                if let Some(mut pipe) = child.stdout.take() {
                    use std::io::Read;
                    let _ = pipe.read_to_string(&mut out);
                }
                return Some(out);
            }
            Ok(Some(_)) => return None,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

/// Wait cap for keychain commands.
const KEYCHAIN_TIMEOUT_SECS: u64 = 5;

/// The single entry point to the platform keychain. macOS: `security add-generic-password -U -s joyczl -a <account>
/// -w <key>`；Linux: `secret-tool store` reads from stdin (**never argv** — argv is visible
/// to all)；Windows: Credential Manager（keyring crate，CredWrite/CredRead）。
fn os_keychain_put(account: &str, secret: &str) -> bool {
    if cfg!(target_os = "macos") {
        return run_limited(
            std::process::Command::new("security").args([
                "add-generic-password",
                "-U",
                "-s",
                SERVICE,
                "-a",
                account,
                "-w",
                secret,
            ]),
            None,
        )
        .is_some();
    }
    if cfg!(target_os = "linux") {
        // stdin carries the value: **never argv** — argv is visible to every process on the machine.
        return run_limited(
            std::process::Command::new("secret-tool").args([
                "store",
                "--label",
                &format!("{SERVICE} {account}"),
                "service",
                SERVICE,
                "account",
                account,
            ]),
            Some(secret),
        )
        .is_some();
    }
    #[cfg(target_os = "windows")]
    {
        if win::put(account, secret) {
            return true;
        }
    }
    // The keychain failed (or is unavailable): fall back to a 0600 file — `put`'s return
    // value says where it really landed.
    false
}

fn os_keychain_get(account: &str) -> Option<String> {
    if cfg!(target_os = "macos") {
        return run_limited(
            std::process::Command::new("security").args([
                "find-generic-password",
                "-s",
                SERVICE,
                "-a",
                account,
                "-w",
            ]),
            None,
        )
        .map(|out| out.trim().to_string())
        .filter(|s| !s.is_empty());
    }
    if cfg!(target_os = "linux") {
        return run_limited(
            std::process::Command::new("secret-tool")
                .args(["lookup", "service", SERVICE, "account", account]),
            None,
        )
        .map(|out| out.trim().to_string())
        .filter(|s| !s.is_empty());
    }
    #[cfg(target_os = "windows")]
    {
        return win::get(account);
    }
    #[cfg(not(target_os = "windows"))]
    None
}

fn os_keychain_remove(account: &str) {
    if cfg!(target_os = "macos") {
        let _ = run_limited(
            std::process::Command::new("security").args([
                "delete-generic-password",
                "-s",
                SERVICE,
                "-a",
                account,
            ]),
            None,
        );
    }
    if cfg!(target_os = "linux") {
        let _ = run_limited(
            std::process::Command::new("secret-tool")
                .args(["clear", "service", SERVICE, "account", account]),
            None,
        );
    }
    #[cfg(target_os = "windows")]
    {
        win::remove(account);
    }
}

// ---- Windows Credential Manager（keyring crate）----------------------------------------------

/// Compiled on Windows only. The command-line paths on macOS and Linux have shipped, so they
/// are not being replaced; Windows previously had the command-line embarrassment of
/// "writes fine, cannot read back" (see limitations), and `keyring` goes through
/// CredWrite/CredRead, which works both ways.
#[cfg(target_os = "windows")]
mod win {
    use keyring::Entry;

    /// A keyring call blocks the OS thread, so it is wrapped in a 5-second timeout — the same
    /// discipline as `security`/`secret-tool`: a credentials call freezing the app-server is
    /// not acceptable. Timeout = failure, and it falls back to the file.
    fn limited<T>(
        account: &str,
        op: impl FnOnce(&Entry) -> Result<T, keyring::Error> + Send + 'static,
    ) -> Option<T>
    where
        // The result has to cross the channel back to the calling thread.
        T: Send + 'static,
    {
        let entry = Entry::new(super::SERVICE, account).ok()?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(op(&entry).ok());
        });
        rx.recv_timeout(std::time::Duration::from_secs(super::KEYCHAIN_TIMEOUT_SECS))
            .ok()
            .flatten()
    }

    pub fn put(account: &str, secret: &str) -> bool {
        // The closure must be 'static (it moves into a thread), so the borrowed secret is
        // first turned into an owned value.
        let secret = secret.to_string();
        limited(account, move |entry| entry.set_password(&secret)).is_some()
    }

    pub fn get(account: &str) -> Option<String> {
        limited(account, |entry| entry.get_password())
    }

    pub fn remove(account: &str) -> bool {
        limited(account, |entry| entry.delete_credential()).is_some()
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod models_tests {
    use super::*;

    fn profile(id: &str, provider: &str) -> ModelProfile {
        ModelProfile {
            id: id.to_string(),
            provider: provider.to_string(),
            model: None,
            small_model: None,
            base_url: None,
            key_source: None,
            vision: None,
            capabilities: None,
        }
    }

    /// The Vault trait is the only seam: a full store/read/delete round works through
    /// `&mut dyn SecretVault` too — a future server KMS backend plugs in here, and callers
    /// (apply_profile and friends) change nothing.
    #[test]
    fn the_vault_trait_is_the_only_seam() {
        let home = Path::new("/tmp/joyczl-vault-trait-test");
        let mut vault: Box<dyn SecretVault> = Box::new(SecretStore::memory());
        assert_eq!(vault.get(home, "llm:ds"), None);
        assert_eq!(
            vault.put(home, "llm:ds", "sk-test").expect("store"),
            "memory"
        );
        assert_eq!(vault.get(home, "llm:ds").as_deref(), Some("sk-test"));
        vault.remove(home, "llm:ds");
        assert_eq!(vault.get(home, "llm:ds"), None);
    }

    /// Memory backend: **never touches the system keychain or disk** — CI must never see auth prompts.
    #[test]
    fn the_memory_backend_never_touches_the_disk() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = SecretStore::memory();
        assert_eq!(store.put(dir.path(), "ds", "sk-1").unwrap(), "memory");
        assert_eq!(store.get(dir.path(), "ds").as_deref(), Some("sk-1"));
        assert!(
            !secrets_path(dir.path()).exists(),
            "the memory backend must not write secrets.json"
        );
    }

    /// File fallback: stores and reads back, with (Unix) permissions tightened to 0600.
    #[test]
    fn the_file_backend_stores_and_reads_back() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = SecretStore::file_only();
        assert_eq!(store.put(dir.path(), "ds", "sk-2").unwrap(), "file");
        assert_eq!(store.get(dir.path(), "ds").as_deref(), Some("sk-2"));
        store.remove(dir.path(), "ds");
        assert_eq!(store.get(dir.path(), "ds"), None);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut store = SecretStore::file_only();
            store.put(dir.path(), "ds", "sk-3").unwrap();
            let mode = std::fs::metadata(secrets_path(dir.path()))
                .expect("the file is there")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "the plaintext fallback should be 0600, actually {mode:o}"
            );
        }
    }

    /// Overwriting a same-id profile **keeps its key_source** — no forced key re-entry.
    #[test]
    fn upserting_keeps_the_existing_key_pointer() {
        let mut models = ModelsFile::default();
        upsert(
            &mut models,
            ModelProfile {
                key_source: Some("keychain".to_string()),
                ..profile("ds", "deepseek")
            },
        );
        upsert(
            &mut models,
            ModelProfile {
                model: Some("deepseek-reasoner".to_string()),
                ..profile("ds", "deepseek")
            },
        );
        assert_eq!(
            models.profiles.len(),
            1,
            "the same id should overwrite, not add a second entry"
        );
        assert_eq!(
            models.profiles[0].model.as_deref(),
            Some("deepseek-reasoner")
        );
        assert_eq!(
            models.profiles[0].key_source.as_deref(),
            Some("keychain"),
            "not passing a key must not clear the existing credential pointer"
        );
    }

    /// Switching and removing: active stays consistent; removing it leaves no dangling pointer.
    #[test]
    fn switching_and_removing_keep_active_consistent() {
        let mut models = ModelsFile::default();
        upsert(&mut models, profile("ds", "deepseek"));
        upsert(&mut models, profile("oa", "openai"));
        models.active = Some("ds".to_string());
        assert_eq!(
            active_profile(&models).map(|p| p.provider.as_str()),
            Some("deepseek")
        );

        models.active = Some("oa".to_string());
        assert_eq!(active_profile(&models).map(|p| p.id.as_str()), Some("oa"));

        assert!(remove(&mut models, "oa"));
        assert!(
            models.active.is_none(),
            "deleting the active profile must not leave an `active` that points at nothing"
        );
        assert!(active_profile(&models).is_none());
        assert!(!remove(&mut models, "not-there"));
    }

    /// Applying into Settings: provider/model/base_url in place, key from the credential backend.
    #[test]
    fn applying_the_active_profile_fills_settings_including_the_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = SecretStore::memory();
        store.put(dir.path(), "ds", "sk-from-store").unwrap();

        let mut models = ModelsFile::default();
        upsert(
            &mut models,
            ModelProfile {
                model: Some("deepseek-flash".to_string()),
                small_model: Some("deepseek-flash".to_string()),
                base_url: Some("https://api.deepseek.com/v1".to_string()),
                key_source: Some("keychain".to_string()),
                ..profile("ds", "deepseek")
            },
        );
        models.active = Some("ds".to_string());

        let mut settings = crate::Settings::default();
        apply_active(&models, dir.path(), &mut store, &mut settings);
        assert_eq!(settings.provider, "deepseek");
        assert_eq!(settings.model.as_deref(), Some("deepseek-flash"));
        assert_eq!(
            settings.base_url.as_deref(),
            Some("https://api.deepseek.com/v1")
        );
        assert_eq!(settings.api_key.as_deref(), Some("sk-from-store"));
    }

    /// An `env:` source reads the environment directly, **never copied into any file**.
    #[test]
    fn an_env_key_source_is_read_from_the_environment() {
        let dir = tempfile::tempdir().expect("temp dir");
        let var = "JOY_TEST_MODELS_KEY";
        std::env::set_var(var, "sk-from-env");
        let mut models = ModelsFile::default();
        upsert(
            &mut models,
            ModelProfile {
                key_source: Some(format!("env:{var}")),
                ..profile("env", "deepseek")
            },
        );
        models.active = Some("env".to_string());
        let mut settings = crate::Settings::default();
        apply_active(
            &models,
            dir.path(),
            &mut SecretStore::memory(),
            &mut settings,
        );
        assert_eq!(settings.api_key.as_deref(), Some("sk-from-env"));
        assert!(
            !secrets_path(dir.path()).exists(),
            "the env source must not copy the key into a file"
        );
        std::env::remove_var(var);
    }

    /// No active profile leaves Settings alone — no config in the file ≠ clearing the config.
    #[test]
    fn no_active_profile_leaves_settings_alone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut settings = crate::Settings {
            provider: "ollama".to_string(),
            ..crate::Settings::default()
        };
        apply_active(
            &ModelsFile::default(),
            dir.path(),
            &mut SecretStore::memory(),
            &mut settings,
        );
        assert_eq!(settings.provider, "ollama");
    }

    /// `vision` follows the config: unset = whatever the vendor defaults to, set = this
    /// profile decides. One vendor ships both vision and text-only models, so this is the
    /// only place that can say which is which.
    #[test]
    fn applying_a_profile_carries_its_vision_declaration() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = SecretStore::memory();
        let mut settings = crate::Settings::default();
        assert_eq!(settings.vision, None, "follows the vendor by default");

        let mut models = ModelsFile::default();
        upsert(
            &mut models,
            ModelProfile {
                vision: Some(true),
                ..profile("ds", "deepseek")
            },
        );
        models.active = Some("ds".to_string());
        apply_active(&models, dir.path(), &mut store, &mut settings);
        assert_eq!(
            settings.vision,
            Some(true),
            "the declaration must reach the runtime settings"
        );

        upsert(
            &mut models,
            ModelProfile {
                vision: Some(false),
                ..profile("ds", "deepseek")
            },
        );
        apply_active(&models, dir.path(), &mut store, &mut settings);
        assert_eq!(
            settings.vision,
            Some(false),
            "switching to a text-only model must take the capability back"
        );
    }

    /// Read/write round trip: the file holds only config and credential **pointers**, never keys.
    #[test]
    fn the_file_round_trips_and_never_contains_the_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut models = ModelsFile::default();
        upsert(
            &mut models,
            ModelProfile {
                key_source: Some("keychain".to_string()),
                ..profile("ds", "deepseek")
            },
        );
        models.active = Some("ds".to_string());
        save_models(dir.path(), &models).expect("write");

        let raw = std::fs::read_to_string(models_path(dir.path())).expect("read");
        assert!(
            !raw.contains("sk-"),
            "no key may appear in models.json: {raw}"
        );

        let back = load_models(dir.path());
        assert_eq!(back.active.as_deref(), Some("ds"));
        assert_eq!(back.profiles.len(), 1);
        assert_eq!(back.profiles[0].provider, "deepseek");
    }
}
