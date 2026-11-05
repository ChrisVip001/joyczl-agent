//! Multi-user on one box: one ops process, N users, each with a complete home.
//!
//! Two ready-made answers: **per-tenant profile** (each tenant gets a HOME — config,
//! memory and credentials are isolated by nature; `JOY_HOME` is already the root of all
//! state) and **per-session locks** (concurrency split by key). Admission is an account:
//! register/login on the web issues a **device token** (登录即配对 — logging in from a
//! device pairs it); the CLI `joy hub adduser` path keeps working unchanged.
//!
//! ```text
//!   <root>/users.json          v2 账目：user → { password credential, devices: {name → token} }
//!   <root>/<user>/             one home per user (state.db, models.json, the full jobs set)
//!   <root>/<user>/app-server.lock   the guard lock is already per-home; holds by nature
//! ```
//!
//! Password hashing is PBKDF2-HMAC-SHA256 (ring — already in the tree via rustls),
//! salted, constant-time verified. No reset flow: losing a password is a CLI matter
//! (re-adduser rotates the device token, web re-register sets a new password).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use ring::pbkdf2;
use serde::{Deserialize, Serialize};

use crate::client::AppServer;

/// Username rules: lowercase alphanumerics and hyphens, length ≤ 32. Not aesthetics —
/// the name becomes a filesystem path; `../` and absolute paths must never get in at load.
pub fn valid_user(name: &str) -> bool {
    let bytes = name.as_bytes();
    !name.is_empty()
        && name.len() <= 32
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// Device-name rules: visible characters only, ≤ 32 chars. The name is a JSON key and a
/// label — it never touches the filesystem, so Unicode ("我的手机") is fine.
pub fn valid_device(name: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty() && trimmed.chars().count() <= 32 && trimmed.chars().all(|c| !c.is_control())
}

/// PBKDF2 iterations: OWASP's PBKDF2-SHA256 guidance (~210k). Debug builds pay it too —
/// auth tests keep the count small by unit-testing the table, not the KDF cost.
/// PBKDF2 iterations: OWASP's PBKDF2-SHA256 guidance (~210k).
fn pbkdf2_iters() -> std::num::NonZeroU32 {
    std::num::NonZeroU32::new(210_000).expect("210_000 非 0")
}

/// Password → stored credential (hex salt + hex hash). Salt is 16 random bytes.
fn hash_password(password: &str) -> Result<Credential> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|e| anyhow::anyhow!("系统随机源读不了：{e}"))?;
    let mut hash = [0u8; 32];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        pbkdf2_iters(),
        &salt,
        password.as_bytes(),
        &mut hash,
    );
    Ok(Credential {
        salt: hex(&salt),
        hash: hex(&hash),
    })
}

fn password_ok(password: &str, credential: &Credential) -> bool {
    let salt = match unhex(&credential.salt) {
        Some(bytes) => bytes,
        None => return false,
    };
    let Some(expected) = unhex(&credential.hash) else {
        return false;
    };
    pbkdf2::verify(
        pbkdf2::PBKDF2_HMAC_SHA256,
        pbkdf2_iters(),
        &salt,
        password.as_bytes(),
        &expected,
    )
    .is_ok()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

/// One stored credential: PBKDF2 salt + hash, both hex.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Credential {
    salt: String,
    hash: String,
}

/// One user: optional web credential + its paired devices (name → token).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Account {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential: Option<Credential>,
    #[serde(default)]
    devices: HashMap<String, String>,
}

/// Registration / login verdicts. The HTTP layer maps these to status codes; the messages
/// are for humans and safe to show.
#[derive(Debug)]
pub enum AuthError {
    /// Registration input broke a rule — the message says which one (safe to show).
    BadInput(&'static str),
    /// Login credentials rejected — one honest line, never which half was wrong.
    Invalid(&'static str),
    /// Username taken.
    Exists,
    /// User table full (`max_open` doubles as the seat count).
    Full,
    /// First user must register from loopback (or via `joy hub adduser`) — otherwise
    /// whoever finds a fresh internet-facing hub first becomes its owner.
    Closed,
    /// Token generation failed (the RNG refused) — not the user's fault.
    Internal,
}

/// The account table. Persisted to `<root>/users.json` (0600). On disk it is always the
/// v2 shape (`{"users": {...}}`); the flat v1 shape (`{user: token}`) is migrated on load.
#[derive(Debug, Default)]
pub struct Users {
    users: Vec<(String, Account)>,
}

impl Users {
    /// Load. Missing file = empty table (the first web registration or adduser creates it);
    /// entries with invalid names are rejected outright — loading a bad name opens the door
    /// to path traversal.
    pub fn load(path: &Path) -> Result<Users> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Users::default());
            }
            Err(e) => return Err(e).context(format!("users.json 读不了：{}", path.display())),
        };
        let raw: serde_json::Value =
            serde_json::from_str(&text).context("users.json 不是合法 JSON")?;
        // v2: {"users": {name: account}}. v1 (flat user→token) migrates in memory: the
        // token becomes device "cli" and no credential — the holder keeps access, the
        // web login appears after they register a password (same username or new).
        let accounts: HashMap<String, Account> = if raw.get("users").is_some() {
            serde_json::from_value(raw["users"].clone()).context("users.json v2 字段不合法")?
        } else {
            let flat: HashMap<String, String> =
                serde_json::from_value(raw).context("users.json 不是合法 JSON")?;
            flat.into_iter()
                .map(|(user, token)| {
                    (
                        user,
                        Account {
                            credential: None,
                            devices: HashMap::from([("cli".to_string(), token)]),
                        },
                    )
                })
                .collect()
        };
        for name in accounts.keys() {
            anyhow::ensure!(
                valid_user(name),
                "users.json 里的用户名 '{name}' 不合格（只许小写字母数字与连字符，≤32）"
            );
        }
        Ok(Users {
            users: accounts.into_iter().collect(),
        })
    }

    /// Persist: write a temp file then rename, mode 0600 (unix).
    pub fn save(&self, path: &Path) -> Result<()> {
        let map: HashMap<&String, &Account> = self.users.iter().map(|(u, a)| (u, a)).collect();
        let text = serde_json::to_string_pretty(&serde_json::json!({ "users": map }))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).context("users.json 写不进去")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, path).context("users.json 换名失败")?;
        Ok(())
    }

    /// CLI path (adduser): upsert the "cli" device, keeping any existing credential —
    /// rotating a token must not erase a password.
    pub fn add(&mut self, user: &str, token: &str) {
        let account = match self.users.iter_mut().find(|(name, _)| name == user) {
            Some((_, account)) => account,
            None => {
                self.users.push((user.to_string(), Account::default()));
                let (_, account) = self.users.last_mut().expect("刚 push 的");
                account
            }
        };
        account.devices.insert("cli".to_string(), token.to_string());
    }

    pub fn len(&self) -> usize {
        self.users.len()
    }

    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }

    /// token → user. Constant-time compare per entry; hit count leaks nothing (full scan).
    pub fn verify(&self, token: &str) -> Option<String> {
        let mut found = None;
        for (user, account) in &self.users {
            if account
                .devices
                .values()
                .any(|t| crate::constant_time_eq(token.as_bytes(), t.as_bytes()))
            {
                found = Some(user.clone());
            }
        }
        found
    }

    /// Register: validate → seat check → store credential + first device token.
    /// `token` comes from the caller (the RNG failure stays the caller's anyhow).
    fn register(
        &mut self,
        user: &str,
        password: &str,
        device: &str,
        token: &str,
        seats: usize,
    ) -> Result<(), AuthError> {
        if !valid_user(user) {
            return Err(AuthError::BadInput(
                "用户名只许小写字母、数字与连字符，≤32 字符",
            ));
        }
        if password.len() < 8 {
            return Err(AuthError::BadInput("密码至少 8 个字符"));
        }
        if !valid_device(device) {
            return Err(AuthError::BadInput("设备名要是 1–32 个可见字符"));
        }
        if self.users.iter().any(|(name, _)| name == user) {
            return Err(AuthError::Exists);
        }
        if self.users.len() >= seats {
            return Err(AuthError::Full);
        }
        let credential = hash_password(password).map_err(|_| AuthError::Internal)?;
        self.users.push((
            user.to_string(),
            Account {
                credential: Some(credential),
                devices: HashMap::from([(device.trim().to_string(), token.to_string())]),
            },
        ));
        Ok(())
    }

    /// Verify a password. Unknown user also runs one PBKDF2 pass against a dummy
    /// credential — user enumeration must not be a timing oracle.
    fn password_ok(&self, user: &str, password: &str) -> bool {
        let credential = self
            .users
            .iter()
            .find(|(name, _)| name == user)
            .and_then(|(_, account)| account.credential.clone());
        match credential {
            Some(credential) => password_ok(password, &credential),
            None => {
                let _ = password_ok(
                    password,
                    &Credential {
                        salt: "00000000000000000000000000000000".to_string(),
                        hash: "0000000000000000000000000000000000000000000000000000000000000000"
                            .to_string(),
                    },
                );
                false
            }
        }
    }

    /// The device's token: reuse when this device already paired, otherwise admit the new
    /// one. Returns the token and whether the table changed (caller persists on change).
    fn device_token(&mut self, user: &str, device: &str, fresh: &str) -> Option<(String, bool)> {
        let account = self
            .users
            .iter_mut()
            .find(|(name, _)| name == user)
            .map(|(_, account)| account)?;
        let name = device.trim().to_string();
        if let Some(existing) = account.devices.get(&name) {
            return Some((existing.clone(), false));
        }
        account.devices.insert(name, fresh.to_string());
        Some((fresh.to_string(), true))
    }
}

/// One pool entry: instance + last-used time. **Eviction decisions never guess about
/// concurrency from timestamps** — use `Arc::strong_count`: only the pool holding it
/// (== 1) means idle; an instance in use by a request is pinned and cannot be closed.
struct PoolEntry {
    app: Arc<AppServer>,
    last_used: Instant,
}

/// Pool decision one: whom to close — the least recently used among idle (`idle`, i.e.
/// only the pool holds it). All pinned → None (soft cap: over-opening beats erroring requests).
pub(crate) fn lru_victim(entries: &[(String, Instant, bool)]) -> Option<String> {
    entries
        .iter()
        .filter(|(_, _, idle)| *idle)
        .min_by_key(|(_, last_used, _)| *last_used)
        .map(|(user, _, _)| user.clone())
}

/// Pool decision two: the list of idle-expired entries. `idle_secs` 0 means the policy is
/// off (empty list); when `now` is earlier than epoch+idle (just booted), `checked_sub`
/// fails → close nothing (better to keep too much than close wrongly).
pub(crate) fn stale_entries(
    entries: &[(String, Instant, bool)],
    idle_secs: u64,
    now: Instant,
) -> Vec<String> {
    if idle_secs == 0 {
        return Vec::new();
    }
    let Some(deadline) = now.checked_sub(std::time::Duration::from_secs(idle_secs)) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter(|(_, last_used, idle)| *idle && *last_used < deadline)
        .map(|(user, _, _)| user.clone())
        .collect()
}

/// Multi-user host: resolve token → lazily spawn that user's app-server.
///
/// Three pool policies: **slot cap** (`JOY_HUB_MAX_OPEN`; each instance is a subprocess),
/// **LRU/idle eviction** (`JOY_HUB_IDLE_SECS`, 0 = never close by idleness), and
/// **in-use is never evicted**. The cap is soft: when everything is pinned, prefer
/// over-opening over turning a request into an error — that is real concurrency,
/// not idle pile-up.
pub struct Hub {
    root: PathBuf,
    users: Mutex<Users>,
    clients: Mutex<HashMap<String, PoolEntry>>,
    max_open: usize,
    idle_secs: u64,
    /// Whether the *first* user may self-register. On a loopback bind: yes (it's your
    /// machine). On a public bind: no — the CLI `adduser` creates the owner, otherwise
    /// whoever finds the fresh hub first claims it. Later registrations just need a seat.
    first_registration_open: Mutex<bool>,
}

impl Hub {
    /// Open a hub. An empty table is fine now — the first user registers from the web
    /// (loopback) or via `joy hub adduser`.
    pub fn open(root: &Path) -> Result<Hub> {
        Self::open_with(root, 32, 900)
    }

    /// Knobs passed explicitly (serve_hub gets them from config; tests use small values).
    pub fn open_with(root: &Path, max_open: usize, idle_secs: u64) -> Result<Hub> {
        let path = root.join("users.json");
        let users = Users::load(&path)?;
        if users.is_empty() {
            eprintln!(
                "(joy) hub：{} 里还没有用户。本机打开驾驶舱即可注册第一个账号，\
                 或 `joy hub adduser <名字> --root {}`。",
                path.display(),
                root.display()
            );
        }
        Ok(Hub {
            root: root.to_path_buf(),
            users: Mutex::new(users),
            clients: Mutex::new(HashMap::new()),
            max_open: max_open.max(1),
            idle_secs,
            first_registration_open: Mutex::new(true),
        })
    }

    /// Whether the first user may self-register (public binds close it; see field doc).
    pub fn permit_first_registration(&self, allowed: bool) {
        *self
            .first_registration_open
            .lock()
            .expect("注册门锁不该中毒") = allowed;
    }

    /// Web registration: seat check + first-user policy + credential + first device.
    /// Persists immediately — a hub restart must not lose an account.
    pub fn register(&self, user: &str, password: &str, device: &str) -> Result<String, AuthError> {
        let token = generate_token().map_err(|_| AuthError::Internal)?;
        let mut users = self.users.lock().expect("users 锁不该中毒");
        if users.is_empty()
            && !*self
                .first_registration_open
                .lock()
                .expect("注册门锁不该中毒")
        {
            return Err(AuthError::Closed);
        }
        users.register(user, password, device, &token, self.max_open)?;
        users
            .save(&self.root.join("users.json"))
            .map_err(|_| AuthError::Internal)?;
        Ok(token)
    }

    /// Web login: verify the password, pair the device (idempotent — same device name
    /// returns the same token), persist when a new device joined.
    pub fn login(&self, user: &str, password: &str, device: &str) -> Result<String, AuthError> {
        if !valid_device(device) {
            return Err(AuthError::Invalid("设备名要是 1–32 个可见字符"));
        }
        let fresh = generate_token().map_err(|_| AuthError::Internal)?;
        let mut users = self.users.lock().expect("users 锁不该中毒");
        if !users.password_ok(user, password) {
            // A short, constant-ish delay: online guessing pays wall-clock even if the
            // KDF is somehow fast. Not a rate limiter — that is documented, not solved.
            std::thread::sleep(std::time::Duration::from_millis(200));
            return Err(AuthError::Invalid("用户名或密码不对"));
        }
        let Some((token, added)) = users.device_token(user, device, &fresh) else {
            return Err(AuthError::Invalid("用户名或密码不对"));
        };
        if added {
            users
                .save(&self.root.join("users.json"))
                .map_err(|_| AuthError::Internal)?;
        }
        Ok(token)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn user_count(&self) -> usize {
        self.users.lock().expect("users 锁不该中毒").len()
    }

    /// A user's home (lazy creation is left to app-server's ensure_home).
    pub fn home(&self, user: &str) -> PathBuf {
        self.root.join(user)
    }

    /// Bearer token → user. Malformed header / no match = None.
    pub fn resolve(&self, authorization: Option<&str>) -> Option<String> {
        let token = authorization?
            .strip_prefix("Bearer ")
            .map(str::trim)
            .filter(|t| !t.is_empty())?;
        self.users.lock().expect("users 锁不该中毒").verify(token)
    }

    /// This user's app-server, spawned if absent (lazy — cold users cost no memory).
    /// The guard lock is per-home: two hubs do not fight, and a hub is also blocked
    /// from the same home as a manually started `joy app-server`.
    pub async fn client(&self, user: &str) -> Result<Arc<AppServer>> {
        let mut clients = self.clients.lock().expect("clients 锁不该中毒");
        if let Some(entry) = clients.get_mut(user) {
            entry.last_used = Instant::now();
            return Ok(Arc::clone(&entry.app));
        }

        // Pool full: close the "idle longest" first. Idle = only the pool holds it — an
        // instance serving a request has strong_count > 1, pinned by nature. If all are
        // pinned, close nothing (soft cap): over-opening is real concurrency, not idleness.
        while clients.len() >= self.max_open {
            let snapshot: Vec<_> = clients
                .iter()
                .map(|(u, e)| (u.clone(), e.last_used, Arc::strong_count(&e.app) == 1))
                .collect();
            let Some(victim) = lru_victim(&snapshot) else {
                break;
            };
            let evicted = clients.remove(&victim).expect("刚选出来的受害者");
            eprintln!(
                "(joy) hub：用户 {victim} 的 app-server 闲置已关（池满 {}）",
                self.max_open
            );
            drop(evicted); // kill_on_drop — the child process exits here.
        }

        // Idle sweep: even below the cap, close entries idle too long (0 = policy off).
        if self.idle_secs > 0 {
            let now = Instant::now();
            let snapshot: Vec<_> = clients
                .iter()
                .map(|(u, e)| (u.clone(), e.last_used, Arc::strong_count(&e.app) == 1))
                .collect();
            for victim in stale_entries(&snapshot, self.idle_secs, now) {
                let evicted = clients.remove(&victim).expect("刚选出来的受害者");
                eprintln!(
                    "(joy) hub：用户 {victim} 的 app-server 闲置已关（超 {} 秒）",
                    self.idle_secs
                );
                drop(evicted);
            }
        }

        // No spawn under the lock: build outside; if someone else raced us in, use theirs.
        let spawned = AppServer::spawn(&self.home(user))?;
        let app = Arc::new(spawned);
        let entry = clients.entry(user.to_string()).or_insert(PoolEntry {
            app,
            last_used: Instant::now(),
        });
        entry.last_used = Instant::now();
        Ok(Arc::clone(&entry.app))
    }

    /// Number of instances currently in the pool (for tests and observation).
    pub fn open_count(&self) -> usize {
        self.clients.lock().expect("clients 锁不该中毒").len()
    }
}

/// Generate a token: 32 random bytes → 64 hex characters.
pub fn generate_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("系统随机源读不了：{e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
