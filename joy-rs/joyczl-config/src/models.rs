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

/// 秘密的唯一收口：LLM key、连接器 token、MCP token 迟早都从这条缝过。
///
/// 现在的实现是 [`SecretStore`]（钥匙串 / 0600 文件 / 内存）。留着这个 trait 是为了
/// 「第二个后端」——服务器形态的 KMS 信封出现时只加实现，`apply_profile` 这些调用方
/// 一行不改。连接器与 MCP 的 token 文件还没搬进来：它们的轮换/锁语义（0600 + 临时
/// 文件改名 + 跨进程锁）值得单独一轮，搬的时机是产品真出服务器版。
pub trait SecretVault {
    /// 存一条秘密，返回**实际落点**（`keychain` / `file` / `memory`）—— UI 与日志必须
    /// 说真话：绝不暗示在钥匙串其实是明文文件。
    fn put(&mut self, home: &Path, account: &str, secret: &str) -> Result<&'static str, String>;

    /// 取回。`None` = 没存过（比如这个档案/服务根本不需要 key）。
    fn get(&mut self, home: &Path, account: &str) -> Option<String>;

    /// 删掉（钥匙串条目与文件兜底一起清）。
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

/// 平台钥匙串的统一入口。macOS: `security add-generic-password -U -s joyczl -a <account>
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
    // 钥匙串失败（或不可用）：回落 0600 文件，put 的返回值会如实说落点。
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

/// 只在 Windows 编译。macOS 与 Linux 的命令行路径已上过线，不换实现；Windows 之前只有
/// 「存得进、读不回」的命令行尴尬（见 limitations），keyring 走 CredWrite/CredRead，两边都通。
#[cfg(target_os = "windows")]
mod win {
    use keyring::Entry;

    /// keyring 的调用是阻塞系统调用，包一层 5 秒超时（与 `security`/`secret-tool` 同一条
    /// 纪律）：凭据操作冻住 app-server 不可接受。超时 = 失败，回落文件。
    fn limited<T>(
        account: &str,
        op: impl FnOnce(&Entry) -> Result<T, keyring::Error> + Send + 'static,
    ) -> Option<T>
    where
        // 结果要穿过通道回到调用线程。
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
        // 闭包必须 'static（要搬进线程），借用的 secret 先落为自有值。
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
        }
    }

    /// Vault trait 是唯一接缝：通过 `&mut dyn SecretVault` 也能完成一整轮存取删 ——
    /// 将来服务器 KMS 后端就从这个口接进来，调用方（apply_profile 一类）零改动。
    #[test]
    fn the_vault_trait_is_the_only_seam() {
        let home = Path::new("/tmp/joyczl-vault-trait-test");
        let mut vault: Box<dyn SecretVault> = Box::new(SecretStore::memory());
        assert_eq!(vault.get(home, "llm:ds"), None);
        assert_eq!(vault.put(home, "llm:ds", "sk-test").expect("存"), "memory");
        assert_eq!(vault.get(home, "llm:ds").as_deref(), Some("sk-test"));
        vault.remove(home, "llm:ds");
        assert_eq!(vault.get(home, "llm:ds"), None);
    }

    /// Memory backend: **never touches the system keychain or disk** — CI must never see auth prompts.
    #[test]
    fn the_memory_backend_never_touches_the_disk() {
        let dir = tempfile::tempdir().expect("临时目录");
        let mut store = SecretStore::memory();
        assert_eq!(store.put(dir.path(), "ds", "sk-1").unwrap(), "memory");
        assert_eq!(store.get(dir.path(), "ds").as_deref(), Some("sk-1"));
        assert!(
            !secrets_path(dir.path()).exists(),
            "内存后端不该写 secrets.json"
        );
    }

    /// File fallback: stores and reads back, with (Unix) permissions tightened to 0600.
    #[test]
    fn the_file_backend_stores_and_reads_back() {
        let dir = tempfile::tempdir().expect("临时目录");
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
                .expect("文件在")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "明文兜底该是 0600，实际 {mode:o}");
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
        assert_eq!(models.profiles.len(), 1, "同 id 该覆盖而不是新增");
        assert_eq!(
            models.profiles[0].model.as_deref(),
            Some("deepseek-reasoner")
        );
        assert_eq!(
            models.profiles[0].key_source.as_deref(),
            Some("keychain"),
            "没给 key 就不该把原来的凭据指针清掉"
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
            "删掉当前那套后不该留一个指不中的 active"
        );
        assert!(active_profile(&models).is_none());
        assert!(!remove(&mut models, "不存在"));
    }

    /// Applying into Settings: provider/model/base_url in place, key from the credential backend.
    #[test]
    fn applying_the_active_profile_fills_settings_including_the_key() {
        let dir = tempfile::tempdir().expect("临时目录");
        let mut store = SecretStore::memory();
        store.put(dir.path(), "ds", "sk-应用").unwrap();

        let mut models = ModelsFile::default();
        upsert(
            &mut models,
            ModelProfile {
                model: Some("deepseek-chat".to_string()),
                small_model: Some("deepseek-chat".to_string()),
                base_url: Some("https://api.deepseek.com/v1".to_string()),
                key_source: Some("keychain".to_string()),
                ..profile("ds", "deepseek")
            },
        );
        models.active = Some("ds".to_string());

        let mut settings = crate::Settings::default();
        apply_active(&models, dir.path(), &mut store, &mut settings);
        assert_eq!(settings.provider, "deepseek");
        assert_eq!(settings.model.as_deref(), Some("deepseek-chat"));
        assert_eq!(
            settings.base_url.as_deref(),
            Some("https://api.deepseek.com/v1")
        );
        assert_eq!(settings.api_key.as_deref(), Some("sk-应用"));
    }

    /// An `env:` source reads the environment directly, **never copied into any file**.
    #[test]
    fn an_env_key_source_is_read_from_the_environment() {
        let dir = tempfile::tempdir().expect("临时目录");
        let var = "JOY_TEST_MODELS_KEY";
        std::env::set_var(var, "sk-环境");
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
        assert_eq!(settings.api_key.as_deref(), Some("sk-环境"));
        assert!(
            !secrets_path(dir.path()).exists(),
            "env 源不该把 key 抄进文件"
        );
        std::env::remove_var(var);
    }

    /// No active profile leaves Settings alone — no config in the file ≠ clearing the config.
    #[test]
    fn no_active_profile_leaves_settings_alone() {
        let dir = tempfile::tempdir().expect("临时目录");
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

    /// `vision` 跟着配置走：不写 = 跟随厂商默认，写了 = 这套模型说了算。
    /// 同一家既有看图模型也有纯文本模型，这是唯一说得准的地方。
    #[test]
    fn applying_a_profile_carries_its_vision_declaration() {
        let dir = tempfile::tempdir().expect("临时目录");
        let mut store = SecretStore::memory();
        let mut settings = crate::Settings::default();
        assert_eq!(settings.vision, None, "默认跟随厂商");

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
        assert_eq!(settings.vision, Some(true), "声明要进运行时设置");

        upsert(
            &mut models,
            ModelProfile {
                vision: Some(false),
                ..profile("ds", "deepseek")
            },
        );
        apply_active(&models, dir.path(), &mut store, &mut settings);
        assert_eq!(settings.vision, Some(false), "换成纯文本模型要收回能力");
    }

    /// Read/write round trip: the file holds only config and credential **pointers**, never keys.
    #[test]
    fn the_file_round_trips_and_never_contains_the_key() {
        let dir = tempfile::tempdir().expect("临时目录");
        let mut models = ModelsFile::default();
        upsert(
            &mut models,
            ModelProfile {
                key_source: Some("keychain".to_string()),
                ..profile("ds", "deepseek")
            },
        );
        models.active = Some("ds".to_string());
        save_models(dir.path(), &models).expect("写");

        let raw = std::fs::read_to_string(models_path(dir.path())).expect("读");
        assert!(!raw.contains("sk-"), "models.json 里不该出现密钥：{raw}");

        let back = load_models(dir.path());
        assert_eq!(back.active.as_deref(), Some("ds"));
        assert_eq!(back.profiles.len(), 1);
        assert_eq!(back.profiles[0].provider, "deepseek");
    }
}
