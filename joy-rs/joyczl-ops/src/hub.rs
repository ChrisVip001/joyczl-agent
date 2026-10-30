//! 多用户单机：一个 ops 进程，N 个用户，各自完整的 home。
//!
//! 学的是两家的现成答案：**hermes 的 profile**（每个租户一套 HOME ——
//! config/记忆/凭据天然隔离，`JOY_HOME` 本来就是全部状态的根）与
//! **nanobot 的 per-session 锁**（并发按 key 分开）。没有账号系统 ——
//! 五家参考实现没有一家有；准入就是 `<root>/users.json` 里的一枚 token，
//! 鉴权逻辑与单用户模式完全同一条（Bearer + 常量时间比较）。
//!
//! ```text
//!   <root>/users.json          user → token（0600，管理员用 joy hub adduser 生成）
//!   <root>/<user>/             每用户一个 home（state.db、models.json、jobs 全套）
//!   <root>/<user>/app-server.lock   守护锁本来就是 per-home 的，天然成立
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};

use crate::client::AppServer;

/// 用户名的规矩：小写字母数字与连字符，长度 ≤ 32。这不是审美 —— 名字要
/// 变成文件系统路径，`../` 和绝对路径必须在**装载时**就进不来。
pub fn valid_user(name: &str) -> bool {
    let bytes = name.as_bytes();
    !name.is_empty()
        && name.len() <= 32
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// `user → token` 表。落盘 `<root>/users.json`（0600）。
#[derive(Debug)]
pub struct Users {
    entries: Vec<(String, String)>,
}

impl Users {
    /// 装载。文件不存在 = 空表（adduser 会建）；名字不合格的条目直接拒载
    /// —— 装一个坏名字等于给路径穿越开门。
    pub fn load(path: &Path) -> Result<Users> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Users {
                    entries: Vec::new(),
                });
            }
            Err(e) => return Err(e).context(format!("users.json 读不了：{}", path.display())),
        };
        let raw: HashMap<String, String> =
            serde_json::from_str(&text).context("users.json 不是合法 JSON")?;
        for name in raw.keys() {
            anyhow::ensure!(
                valid_user(name),
                "users.json 里的用户名 '{name}' 不合格（只许小写字母数字与连字符，≤32）"
            );
        }
        Ok(Users {
            entries: raw.into_iter().collect(),
        })
    }

    /// 落盘：先写临时文件再换名，权限 0600（unix）。
    pub fn save(&self, path: &Path) -> Result<()> {
        let map: HashMap<&String, &String> = self.entries.iter().map(|(u, t)| (u, t)).collect();
        let text = serde_json::to_string_pretty(&map)?;
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

    pub fn add(&mut self, user: &str, token: &str) {
        self.entries.retain(|(name, _)| name != user);
        self.entries.push((user.to_string(), token.to_string()));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// token → user。逐条常量时间比较；命中数量不泄漏（全表扫完才返回）。
    pub fn verify(&self, token: &str) -> Option<String> {
        let mut found = None;
        for (user, expected) in &self.entries {
            if crate::constant_time_eq(token.as_bytes(), expected.as_bytes()) {
                found = Some(user.clone());
            }
        }
        found
    }
}

/// 池里的一个条目：实例 + 最后使用时刻。**驱逐判定不用时间戳对并发做
/// 猜测** —— 用 `Arc::strong_count`：只有池自己持有（== 1）才是闲的，
/// 正被某个请求用着的实例天然 pin 住，不可能被关掉。
struct PoolEntry {
    app: Arc<AppServer>,
    last_used: Instant,
}

/// 池的判定之一：该关谁 —— 闲（`idle`，即只有池自己持有）之中最久未用的。
/// 全都 pin 住 → None（软上限：超开好过把请求变成错误）。
pub(crate) fn lru_victim(entries: &[(String, Instant, bool)]) -> Option<String> {
    entries
        .iter()
        .filter(|(_, _, idle)| *idle)
        .min_by_key(|(_, last_used, _)| *last_used)
        .map(|(user, _, _)| user.clone())
}

/// 池的判定之二：闲置超期的名单。`idle_secs` 为 0 视为策略关闭（空名单）；
/// `now` 早于 epoch+idle（刚开机）时 `checked_sub` 落空 → 不关（宁可多留，
/// 不可误关）。
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

/// 多用户宿主：解析 token → 懒加载该用户的 app-server。
///
/// 池的三条策略抄自 openhuman 的 ProfileHost：**slot 上限**
/// （`JOY_HUB_MAX_OPEN`，每个实例是一个子进程）、**LRU/idle 驱逐**
/// （`JOY_HUB_IDLE_SECS`，0 = 不按闲置关）、**in-use 永不驱逐**。
/// 上限是软的：全都 pin 住时宁可超开也不把请求变成错误 —— 那是真实
/// 并发，不是闲置堆积。
pub struct Hub {
    root: PathBuf,
    users: Mutex<Users>,
    clients: Mutex<HashMap<String, PoolEntry>>,
    max_open: usize,
    idle_secs: u64,
}

impl Hub {
    /// 打开一个 hub。users.json 不存在或为空 → 报错并指路（空 hub 是配置
    /// 错误，不是「等第一个用户」）。
    pub fn open(root: &Path) -> Result<Hub> {
        Self::open_with(root, 32, 900)
    }

    /// 旋钮显式传入（serve_hub 从配置来；测试用小值压出驱逐行为）。
    pub fn open_with(root: &Path, max_open: usize, idle_secs: u64) -> Result<Hub> {
        let path = root.join("users.json");
        let users = Users::load(&path)?;
        anyhow::ensure!(
            !users.is_empty(),
            "{} 里一个用户都没有。先用 `joy hub adduser <名字> --root {}` 建一个。",
            path.display(),
            root.display()
        );
        Ok(Hub {
            root: root.to_path_buf(),
            users: Mutex::new(users),
            clients: Mutex::new(HashMap::new()),
            max_open: max_open.max(1),
            idle_secs,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn user_count(&self) -> usize {
        self.users.lock().expect("users 锁不该中毒").len()
    }

    /// 某用户的 home（懒创建留给 app-server 的 ensure_home）。
    pub fn home(&self, user: &str) -> PathBuf {
        self.root.join(user)
    }

    /// Bearer token → user。头部形状不对/没人对上 = None。
    pub fn resolve(&self, authorization: Option<&str>) -> Option<String> {
        let token = authorization?
            .strip_prefix("Bearer ")
            .map(str::trim)
            .filter(|t| !t.is_empty())?;
        self.users.lock().expect("users 锁不该中毒").verify(token)
    }

    /// 该用户的 app-server，没有就拉起（懒加载 —— 用到才起，冷用户不占内存）。
    /// 守护锁是 per-home 的：两个 hub 不会打架，hub 与手工 `joy app-server`
    /// 同一 home 也会被挡住。
    pub async fn client(&self, user: &str) -> Result<Arc<AppServer>> {
        let mut clients = self.clients.lock().expect("clients 锁不该中毒");
        if let Some(entry) = clients.get_mut(user) {
            entry.last_used = Instant::now();
            return Ok(Arc::clone(&entry.app));
        }

        // 池满：先关「闲得最久」的。闲 = 只有池自己持有 —— 正在服务请求的
        // 实例 strong_count > 1，天然 pin 住。全都 pin 住就不关（软上限）：
        // 超开的是真实并发，不是闲置堆积，关了会把请求变成错误。
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
            drop(evicted); // kill_on_drop —— 子进程在这里退出。
        }

        // 闲置清扫：上限没满也顺手关掉闲置太久的（0 = 关闭此策略）。
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

        // 不跨锁 spawn：构造放锁外，插回时可能已被别人抢先 —— 那就用他的。
        let spawned = AppServer::spawn(&self.home(user))?;
        let app = Arc::new(spawned);
        let entry = clients.entry(user.to_string()).or_insert(PoolEntry {
            app,
            last_used: Instant::now(),
        });
        entry.last_used = Instant::now();
        Ok(Arc::clone(&entry.app))
    }

    /// 池里现在的实例数（测试与观测用）。
    pub fn open_count(&self) -> usize {
        self.clients.lock().expect("clients 锁不该中毒").len()
    }
}

/// 生成一枚 token：32 字节随机 → 64 个 hex 字符。
pub fn generate_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("系统随机源读不了：{e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
