//! `<home>/models.json` —— 多套「厂商 + 模型 + 凭据」配置，随手切换。
//!
//! 与 `settings.json` 的分工：**那一份是运行参数**（轮数、token、超时、闸门），
//! **这一份是用哪家的哪个模型、拿什么 key**。后者要能被用户在界面上改，而且
//! 凭据优先交给系统钥匙串 —— 所以单独一个文件、单独一套协议方法。
//!
//! 三件事在实现里是纪律，不是可选优化：
//!
//! * **凭据默认不落明文**：macOS 走 Keychain（`security`）、Linux 走
//!   `secret-tool`，都失败才退回 `<home>/secrets.json`（0600）。Windows 的
//!   凭据管理器存得进、读不回明文，所以直接走文件 —— 写进 limitations。
//! * **测试绝不碰真钥匙串**：`SecretStore::memory()` / `file_only()` 让单测
//!   在临时目录里跑，不弹系统授权框、不写用户钥匙串。
//! * **profile 只是「配置」**：套不套到当前会话由 `model/use` 决定；文件本身
//!   改了不影响正在跑的那一轮。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 钥匙串里的 service 名（三端共用）。
pub const SERVICE: &str = "joyczl";

/// 一套模型配置。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelProfile {
    /// 用户起的短名（`ds`、`公司-openai`…），切换时就用它。
    pub id: String,
    /// `joyczl-provider` 的 provider id（12 家之一，或 `custom` 配 base_url）。
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
    /// 便宜模型：检索门、consolidation、目标判断器用它。不给就用 provider 默认。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub small_model: Option<String>,
    /// 自建/代理端点。留空用 provider 自己的。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub base_url: Option<String>,
    /// 凭据从哪来：`env:VAR` / `keychain` / `file`；`None` = 这家不用 key
    /// （ollama、LM Studio）。**这里只存指针，不存 key 本身。**
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub key_source: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelsFile {
    /// 当前生效的 profile id。
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
    // active 指向的 id 被删了就当没有 —— 不静默留一个悬空指针。
    models.active.as_deref().and_then(|id| find(models, id))
}

/// 写入（或覆盖同名）一套配置。**没给 key 就保留原 key_source** ——
/// 「切换」「改模型」不该逼用户重发一次密钥。
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

/// 把 active profile 的**配置**套进 Settings（凭据另取，见 `apply_active`）。
/// 不动 `settings.json`：那是运行参数的文件，模型配置有自己的家。
pub fn apply_active(
    models: &ModelsFile,
    home: &Path,
    store: &mut SecretStore,
    settings: &mut crate::Settings,
) {
    let Some(profile) = active_profile(models) else {
        return;
    };
    settings.provider = profile.provider.clone();
    settings.model = profile.model.clone();
    settings.small_model = profile.small_model.clone();
    settings.base_url = profile.base_url.clone();
    settings.api_key = resolve_key(profile, home, store);
}

/// 按 `key_source` 取真正的 key。`env:` 直接读环境变量（**不复制进文件**），
/// `keychain`/`file` 走 `SecretStore`，没有就 None（本地模型）。
pub fn resolve_key(profile: &ModelProfile, home: &Path, store: &mut SecretStore) -> Option<String> {
    let source = profile.key_source.as_deref()?;
    match source.strip_prefix("env:") {
        Some(var) => std::env::var(var).ok().filter(|v| !v.trim().is_empty()),
        None => store.get(home, &profile.id),
    }
}

/// 凭据放哪儿。`Auto` 是默认：能用系统钥匙串就用，用不了退文件。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretBackend {
    Auto,
    File,
    /// 只活在进程内存里：测试用，也供「就是不落盘」的用户显式选。
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

    /// 换后端。**只给测试与「就是不落钥匙串」的场景**：默认 `Auto`，单测要
    /// 钉成 `File`（临时目录）或 `Memory`，绝不能在 CI 里弹系统授权框。
    pub fn set_backend(&mut self, backend: SecretBackend) {
        self.backend = backend;
    }

    /// 存一个 key，返回**实际落在哪**（`keychain` / `file` / `memory`）——
    /// 界面与日志要如实告诉用户，不能让用户以为进了钥匙串其实是明文文件。
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
                if keychain_put(account, secret) {
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
                keychain_get(account).or_else(|| read_file_secret(home, account))
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
                keychain_remove(account);
                let _ = remove_file_secret(home, account);
            }
        }
    }
}

// ---- 文件兜底：`<home>/secrets.json`，0600 -----------------------------------

fn read_secrets(home: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(secrets_path(home))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_secrets(home: &Path, secrets: &HashMap<String, String>) -> Result<(), String> {
    let raw = serde_json::to_string_pretty(secrets).map_err(|e| e.to_string())?;
    std::fs::write(secrets_path(home), raw).map_err(|e| e.to_string())?;
    // 明文兜底，权限能收紧就收紧（Windows 没有 0600 这一说 —— 写进 limitations）。
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

// ---- 系统钥匙串：macOS `security` / Linux `secret-tool` ----------------------

/// 跑一个钥匙串命令，**带超时**。
///
/// `security` / `secret-tool` 在无人值守的环境里可能弹框或干等（CI、远程
/// 会话、没有 GUI 的 ssh）。一次凭据操作把整个 app-server 冻住是不能接受的 ——
/// 超时就当失败，走文件兜底。stdout 非空才算真拿到东西。
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

/// 钥匙串命令的等待上限。
const KEYCHAIN_TIMEOUT_SECS: u64 = 5;

/// macOS：`security add-generic-password -U -s joyczl -a <account> -w <key>`。
/// Linux：`secret-tool store` 从 stdin 读（**不进 argv** —— argv 对所有进程可见）。
fn keychain_put(account: &str, secret: &str) -> bool {
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
        // stdin 传值：**不进 argv** —— argv 对同机所有进程可见。
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
    // Windows 的凭据管理器存得进、读不回明文 —— 直接走文件兜底。
    false
}

fn keychain_get(account: &str) -> Option<String> {
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
    None
}

fn keychain_remove(account: &str) {
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
        }
    }

    /// 内存后端：**不碰系统钥匙串、不落盘** —— CI 里绝不能弹授权框。
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

    /// 文件兜底：能取回，且（Unix）权限收紧到 0600。
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

    /// 覆盖同名配置时**保留原 key_source** —— 改模型不该逼用户重发密钥。
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

    /// 切换与删除：active 跟得上，删掉的 active 不留悬空指针。
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

    /// 套进 Settings：provider/model/base_url 到位，key 从凭据后端取。
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

    /// `env:` 源直接读环境变量，**不复制进任何文件**。
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

    /// 没有 active 时不动 Settings —— 文件里没有配置 ≠ 清空当前配置。
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

    /// 读写往返：文件里只有配置与凭据**指针**，没有 key 本身。
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
