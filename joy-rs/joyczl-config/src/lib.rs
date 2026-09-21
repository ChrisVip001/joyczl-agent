//! `joyczl-config` —— 每个旋钮都是一个环境变量，见仓库根的 `.env.example`。
//!
//! 注意 Joy **不读 `.env` 文件**：那份模板是给人 source 的
//! （`set -a; source .env; set +a`），不是自动加载的。
//!
//! 不搞设置框架：启动时读一次，之后就是不可变的普通结构体。
//! 读得懂这个文件，就读得懂 Joy 能被配置成什么样。

use std::path::PathBuf;

use joyczl_protocol::SettingsView;

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_bool(name: &str) -> bool {
    matches!(env(name).as_deref(), Some("1") | Some("true") | Some("yes"))
}

fn env_int(name: &str, default: i32) -> i32 {
    env(name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_float(name: &str, default: f64) -> f64 {
    env(name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

// ---- 旋钮的边界：唯一来源 ----------------------------------------------------
//
// 两个入口共用这张表：**启动期**（`Settings::validate`，非法值当场退出）与
// **config/write**（`validate_patch_values`，非法补丁整个拒绝）。此前只有后者
// 校验，于是 `JOY_HISTORY_TURNS=-5`、`JOY_EXEC_TIMEOUT=abc` 这类值会在启动时
// 静默变成默认值或零窗口 —— 「配错了」于是变成运行期的惊喜。
//
// 表里同时带补丁字段名与环境变量名：报错时按来源挑一个念给人听。

pub struct Bound {
    /// `SettingsPatch` 里的字段名（`config/write` 路径用）。
    /// `None` = 只能从环境变量来（目前是两个超时）。
    pub patch_name: Option<&'static str>,
    pub env_name: &'static str,
    pub min: i64,
    pub max: i64,
}

pub const BOUNDS: &[Bound] = &[
    Bound {
        patch_name: Some("maxIterations"),
        env_name: "JOY_MAX_ITERATIONS",
        min: 1,
        max: 100,
    },
    Bound {
        patch_name: Some("maxTokens"),
        env_name: "JOY_MAX_TOKENS",
        min: 128,
        max: 200_000,
    },
    Bound {
        patch_name: Some("historyTurns"),
        env_name: "JOY_HISTORY_TURNS",
        min: 0,
        max: 1_000,
    },
    Bound {
        patch_name: Some("consolidateEvery"),
        env_name: "JOY_CONSOLIDATE_EVERY",
        min: 1,
        max: 1_000,
    },
    Bound {
        patch_name: Some("retrievalTopK"),
        env_name: "JOY_RETRIEVAL_TOP_K",
        min: 1,
        max: 100,
    },
    Bound {
        patch_name: None,
        env_name: "JOY_LLM_RETRIES",
        min: 0,
        max: 5,
    },
    Bound {
        patch_name: None,
        env_name: "JOY_LLM_TIMEOUT",
        min: 1,
        max: 3_600,
    },
    Bound {
        patch_name: None,
        env_name: "JOY_EXEC_TIMEOUT",
        min: 1,
        max: 3_600,
    },
];

/// 放行规则的条数与单条长度上限。规则是手写的，几百条或几千字符的「规则」
/// 只可能是一次误粘贴（比如把整个脚本贴进了环境变量）。
const MAX_EXEC_RULES: usize = 64;
const MAX_EXEC_RULE_CHARS: usize = 200;

/// 按补丁字段名或环境变量名查边界并报错。
fn check_bound(key: &str, value: i64) -> Result<(), String> {
    let bound = BOUNDS
        .iter()
        .find(|b| b.env_name == key || b.patch_name == Some(key))
        .expect("边界表里没有这个旋钮 —— 这是代码 bug，不是用户配错了");
    if value < bound.min || value > bound.max {
        return Err(format!(
            "{key} 应该在 {} 到 {} 之间，收到 {value}",
            bound.min, bound.max
        ));
    }
    Ok(())
}

// 只有 PartialEq，没有 Eq：`compact_threshold` 是 f64，而 f64 不是 Eq
// （NaN 让「相等」失去自反性）。除它之外全是整数与布尔，用不上 Eq 的地方
// 就别假装用得上。
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    // ---- LLM：选一个 provider，配它的 key。见 joyczl-provider 的 PROVIDERS。
    pub provider: String,
    /// 显式覆盖：key、endpoint、模型 id。留空就用 provider 自己的默认。
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    /// 便宜模型：检索门和 consolidation 用它。
    pub small_model: Option<String>,

    // ---- Home：Joy 的状态都在这（state.db、SOUL.md、traces/）。
    pub home: PathBuf,

    // ---- Loop 护栏
    /// `JOY_LLM_RETRIES`：限流/临时故障时最多退避重试几次（默认 2，上限 5，
    /// 0 = 关闭）。只重试「再试一次有意义」的错误，而且**每次必发通知** ——
    /// 见 joyczl-provider 的 retry.rs。
    pub llm_retries: i32,
    /// 单次模型调用的超时。挂掉的网络调用绝不能让一轮 turn 静默冻结。
    pub llm_timeout_secs: i64,
    pub max_iterations: i32,
    /// 留够余量：推理模型（kimi-k3、gpt-5.x、gemini-*-pro）会先花输出 token
    /// 思考再作答，cap 太低会 stop_reason=max_tokens 卡在思考里。
    pub max_tokens: i32,
    /// 工作记忆滑窗：只把最近 N 轮塞进 prompt。更老的在 state.db 里，
    /// 靠检索门 + 情景记忆找回来。
    ///
    /// 它是**上限**，不是触发条件：真正决定什么时候压缩的是 token，
    /// 见 `compact_threshold`。
    pub history_turns: i32,
    /// `JOY_CONTEXT_WINDOW`：覆盖 provider 表里的上下文窗口近似值。
    /// 本地模型（ollama / LM Studio）的窗口千差万别，表里的值只是常见默认。
    pub context_window: Option<u32>,
    /// `JOY_COMPACT_THRESHOLD`：用到上下文窗口的多少比例就开始压缩
    /// （默认 0.8）。轮数是上限、token 是闸门 —— 一个长工具输出就能撑爆
    /// 窗口，而轮数看起来还很"安全"。
    pub compact_threshold: f64,

    // ---- 记忆
    /// 每 N 轮新对话才提炼一次事实。
    pub consolidate_every: i32,
    pub retrieval_top_k: i32,

    // ---- 开关
    pub apple_calendar: bool,
    pub google_calendar: bool,
    pub experimental: bool,
    pub graph_workflows: bool,

    // ---- 混合检索（向量那条腿，见 joyczl-provider 的 embed.rs）
    /// `JOY_EMBEDDINGS=1`：检索时把向量相似度与关键词结果融合。
    /// 默认关 —— 它要有 embedding 模型才有意义，且多一次网络调用。
    pub embeddings_enabled: bool,
    /// `JOY_EMBED_MODEL`：embedding 模型名（如 nomic-embed-text）。
    pub embed_model: Option<String>,

    // ---- 执行（`run_command`，见 joyczl-tools 的 exec.rs）
    /// `JOY_EXEC`：开了才会把 run_command 注册进工具表。默认关 ——
    /// 一个能执行命令的助手，开关必须是用户亲手按下的。
    pub exec_enabled: bool,
    /// `JOY_EXEC_ALLOW`：放行规则（逗号分隔，支持末尾 `*` 通配）。
    /// **空表 = 什么都不放行**：默认拒绝，不是默认允许。
    pub exec_allow: Vec<String>,
    /// `JOY_EXEC_TIMEOUT`：单条命令的超时（秒）。
    pub exec_timeout_secs: i64,
    /// `JOY_EXEC_NETWORK`：沙箱里**能不能联网**。默认 **false（断网）** ——
    /// 一条被放行的命令默认不该拥有把数据送出去的能力。
    /// 要让 `cargo test` 这类需要下载的命令跑起来，得显式设成 1。
    pub exec_network: bool,
    /// `JOY_EXEC_WRITABLE_ROOTS`：除了工作目录与 home 之外，还允许写哪些目录
    /// （冒号分隔的绝对路径，必须已存在）。为了 `cargo build` 之类的构建缓存。
    pub exec_writable_roots: Vec<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            provider: "anthropic".to_string(),
            api_key: None,
            base_url: None,
            model: None,
            small_model: None,
            home: PathBuf::from(".joy"),
            llm_retries: 2,
            llm_timeout_secs: 120,
            max_iterations: 10,
            max_tokens: 8192,
            history_turns: 12,
            context_window: None,
            compact_threshold: 0.8,
            consolidate_every: 6,
            retrieval_top_k: 4,
            apple_calendar: false,
            google_calendar: false,
            experimental: false,
            graph_workflows: false,
            embeddings_enabled: false,
            embed_model: None,
            exec_enabled: false,
            exec_allow: Vec::new(),
            exec_timeout_secs: 30,
            exec_network: false,
            exec_writable_roots: Vec::new(),
        }
    }
}

impl Settings {
    /// 从环境变量读。这是唯一的读取路径 —— 不另有配置文件、不另有优先级规则。
    pub fn from_env() -> Self {
        let d = Settings::default();
        Self {
            provider: env("JOY_PROVIDER").unwrap_or(d.provider),
            api_key: env("JOY_API_KEY"),
            base_url: env("JOY_BASE_URL"),
            model: env("JOY_MODEL"),
            small_model: env("JOY_SMALL_MODEL"),
            home: env("JOY_HOME").map(PathBuf::from).unwrap_or(d.home),
            llm_retries: env_int("JOY_LLM_RETRIES", d.llm_retries),
            llm_timeout_secs: env_int("JOY_LLM_TIMEOUT", d.llm_timeout_secs as i32) as i64,
            max_iterations: env_int("JOY_MAX_ITERATIONS", d.max_iterations),
            max_tokens: env_int("JOY_MAX_TOKENS", d.max_tokens),
            history_turns: env_int("JOY_HISTORY_TURNS", d.history_turns),
            context_window: env("JOY_CONTEXT_WINDOW").and_then(|v| v.parse().ok()),
            compact_threshold: env_float("JOY_COMPACT_THRESHOLD", d.compact_threshold),
            consolidate_every: env_int("JOY_CONSOLIDATE_EVERY", d.consolidate_every),
            retrieval_top_k: env_int("JOY_RETRIEVAL_TOP_K", d.retrieval_top_k),
            apple_calendar: env_bool("JOY_APPLE_CALENDAR"),
            google_calendar: env_bool("JOY_GOOGLE_CALENDAR"),
            experimental: env_bool("JOY_EXPERIMENTAL"),
            graph_workflows: env_bool("JOY_GRAPH_WORKFLOWS"),
            embeddings_enabled: env_bool("JOY_EMBEDDINGS"),
            embed_model: env("JOY_EMBED_MODEL"),
            exec_enabled: env_bool("JOY_EXEC"),
            exec_allow: env("JOY_EXEC_ALLOW")
                .map(|raw| {
                    raw.split(',')
                        .map(str::trim)
                        .filter(|rule| !rule.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            exec_timeout_secs: env_int("JOY_EXEC_TIMEOUT", d.exec_timeout_secs as i32) as i64,
            exec_network: env_bool("JOY_EXEC_NETWORK"),
            exec_writable_roots: env("JOY_EXEC_WRITABLE_ROOTS")
                .map(|raw| {
                    std::env::split_paths(&raw)
                        .filter(|p| !p.as_os_str().is_empty())
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// 确保 home 和它的子目录存在。
    pub fn ensure_home(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.home.join("traces"))?;
        std::fs::create_dir_all(self.home.join("outbox"))?;
        std::fs::create_dir_all(self.home.join("skills"))?;
        // 超长命令输出落盘的地方（见 joyczl-tools 的 exec.rs 与 limitations）。
        std::fs::create_dir_all(self.home.join("spill"))?;
        Ok(())
    }

    /// 启动期校验：非法值**当场报错**，不静默变默认值。
    ///
    /// 调用方（`app-server` 的 `open`）拿到 `Err` 就退出并打印原因 ——
    /// 「启动失败」比「跑起来但行为不对」便宜得多，也容易查得多。
    pub fn validate(&self) -> Result<(), String> {
        check_bound("JOY_MAX_ITERATIONS", self.max_iterations as i64)?;
        check_bound("JOY_MAX_TOKENS", self.max_tokens as i64)?;
        check_bound("JOY_HISTORY_TURNS", self.history_turns as i64)?;
        check_bound("JOY_CONSOLIDATE_EVERY", self.consolidate_every as i64)?;
        check_bound("JOY_RETRIEVAL_TOP_K", self.retrieval_top_k as i64)?;
        check_bound("JOY_LLM_RETRIES", self.llm_retries as i64)?;
        check_bound("JOY_LLM_TIMEOUT", self.llm_timeout_secs)?;
        check_bound("JOY_EXEC_TIMEOUT", self.exec_timeout_secs)?;

        // 覆盖窗口时顺手查一条关系：答案的额度不能比窗口还大 —— 那种配置
        // 下模型永远答不完，而表现是「莫名其妙被截断」。
        if let Some(window) = self.context_window {
            if !(1_024..=10_000_000).contains(&window) {
                return Err(format!(
                    "JOY_CONTEXT_WINDOW 应该在 1024 到 10000000 之间，收到 {window}"
                ));
            }
            if self.max_tokens as u32 >= window {
                return Err(format!(
                    "JOY_MAX_TOKENS（{}）不能大于等于 JOY_CONTEXT_WINDOW（{window}）—— \
                     那样连答案都放不下",
                    self.max_tokens
                ));
            }
        }

        // 比例型旋钮单独查（不是整数，进不了 BOUNDS 那张表）。
        if !(0.05..=0.95).contains(&self.compact_threshold) {
            return Err(format!(
                "JOY_COMPACT_THRESHOLD 应该在 0.05 到 0.95 之间，收到 {}",
                self.compact_threshold
            ));
        }

        // 额外可写根：必须是**已存在的绝对目录**。写规则时它们要被塞进沙箱
        // 配置里，一个不存在的路径只会让规则静默失效（seatbelt 认不存在的
        // subpath），于是「我明明放开了却写不进去」。
        for root in &self.exec_writable_roots {
            if !root.is_absolute() {
                return Err(format!(
                    "JOY_EXEC_WRITABLE_ROOTS 里必须是绝对路径，收到 {}",
                    root.display()
                ));
            }
            if !root.is_dir() {
                return Err(format!(
                    "JOY_EXEC_WRITABLE_ROOTS 里的目录不存在：{}",
                    root.display()
                ));
            }
        }

        // 放行规则：空表合法（= 什么都不放行，那是默认）。但表里每一条都得是
        // 一条**能用的**规则 —— 写坏的规则会静默地永不匹配，而命令被拒时
        // 拒因里还列着它，查起来很费劲。
        if self.exec_allow.len() > MAX_EXEC_RULES {
            return Err(format!(
                "JOY_EXEC_ALLOW 最多 {MAX_EXEC_RULES} 条规则，收到 {} 条",
                self.exec_allow.len()
            ));
        }
        for rule in &self.exec_allow {
            let rule = rule.trim();
            if rule.is_empty() {
                return Err("JOY_EXEC_ALLOW 里有空规则 —— 空串不是一条规则".to_string());
            }
            if rule.contains(['\n', '\r']) {
                return Err(format!("JOY_EXEC_ALLOW 的规则不能含换行：{rule:?}"));
            }
            if rule.chars().count() > MAX_EXEC_RULE_CHARS {
                return Err(format!(
                    "JOY_EXEC_ALLOW 的规则最长 {MAX_EXEC_RULE_CHARS} 字符：{rule:?}"
                ));
            }
        }
        Ok(())
    }

    /// `config/read` 的载荷：读出来的就该是完整现状，所以字段全量且非可选。
    pub fn view(&self) -> SettingsView {
        SettingsView {
            provider: self.provider.clone(),
            model: self.model.clone().unwrap_or_default(),
            small_model: self.small_model.clone().unwrap_or_default(),
            max_iterations: self.max_iterations,
            max_tokens: self.max_tokens,
            history_turns: self.history_turns,
            consolidate_every: self.consolidate_every,
            retrieval_top_k: self.retrieval_top_k,
            apple_calendar: self.apple_calendar,
            google_calendar: self.google_calendar,
            experimental: self.experimental,
            graph_workflows: self.graph_workflows,
            home: self.home.display().to_string(),
        }
    }
}

// ---- config/write：补丁的合并、落盘与套用 -----------------------------------
//
// 旋钮的唯一来源仍然是环境变量（`from_env`），这一点不变。dashboard 的
// `config/write` 需要让改动活过重启，于是多了一层**补丁**：写到
// `<home>/settings.json` 的 `SettingsPatch`，启动时叠在环境值之上。
// 补丁里没有的字段 = 交给环境；有 = 显式覆盖。谁最后说话，文件里一目了然。

/// `<home>/settings.json` —— `config/write` 的持久化位置。
pub fn patch_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join("settings.json")
}

/// `config/write` 补丁的**数值**校验：与启动期共用 `BOUNDS`。
///
/// provider 的存在性不在这里查 —— `joyczl-config` 不认识 `PROVIDERS`
/// （那是 provider 层的事），调用方（app-server）把那一步补上。
pub fn validate_patch_values(patch: &joyczl_protocol::SettingsPatch) -> Result<(), String> {
    for (key, value) in [
        ("maxIterations", patch.max_iterations),
        ("maxTokens", patch.max_tokens),
        ("historyTurns", patch.history_turns),
        ("consolidateEvery", patch.consolidate_every),
        ("retrievalTopK", patch.retrieval_top_k),
    ] {
        if let Some(v) = value {
            check_bound(key, v as i64)?;
        }
    }
    Ok(())
}

/// 读已保存的补丁。文件不存在、解析不了都当「没有补丁」：
/// 一个坏掉的配置文件不该让 Joy 起不来，环境变量还在兜底。
pub fn load_patch(home: &std::path::Path) -> joyczl_protocol::SettingsPatch {
    std::fs::read_to_string(patch_path(home))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// 把补丁写到 `<home>/settings.json`。全空的补丁没有可说的，删掉文件，
/// 让 `config/write` 把所有字段改回默认后，「这个目录里没有覆盖」这件事成立。
pub fn save_patch(
    home: &std::path::Path,
    patch: &joyczl_protocol::SettingsPatch,
) -> std::io::Result<()> {
    let path = patch_path(home);
    if patch.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => return Ok(()),
            // 本来就没有：目标已经达成。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
    }
    let raw = serde_json::to_string_pretty(patch).expect("SettingsPatch 序列化");
    std::fs::write(&path, raw)
}

/// 把补丁叠到设置上。补丁里 `Some` 的字段才有发言权；
/// model / small_model 传空串视为「清掉显式覆盖，回到 provider 默认」。
pub fn apply_patch(patch: &joyczl_protocol::SettingsPatch, settings: &mut Settings) {
    let p = patch;
    if let Some(v) = &p.provider {
        settings.provider = v.trim().to_string();
    }
    if let Some(v) = &p.model {
        settings.model = non_empty(v);
    }
    if let Some(v) = &p.small_model {
        settings.small_model = non_empty(v);
    }
    if let Some(v) = p.max_iterations {
        settings.max_iterations = v;
    }
    if let Some(v) = p.max_tokens {
        settings.max_tokens = v;
    }
    if let Some(v) = p.history_turns {
        settings.history_turns = v;
    }
    if let Some(v) = p.consolidate_every {
        settings.consolidate_every = v;
    }
    if let Some(v) = p.retrieval_top_k {
        settings.retrieval_top_k = v;
    }
    if let Some(v) = p.apple_calendar {
        settings.apple_calendar = v;
    }
    if let Some(v) = p.google_calendar {
        settings.google_calendar = v;
    }
    if let Some(v) = p.experimental {
        settings.experimental = v;
    }
    if let Some(v) = p.graph_workflows {
        settings.graph_workflows = v;
    }
}

fn non_empty(v: &str) -> Option<String> {
    let v = v.trim();
    (!v.is_empty()).then(|| v.to_string())
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_values() {
        let s = Settings::default();
        assert_eq!(s.provider, "anthropic");
        assert_eq!(s.max_iterations, 10);
        assert_eq!(s.max_tokens, 8192);
        assert_eq!(s.history_turns, 12);
        assert_eq!(s.consolidate_every, 6);
        assert_eq!(s.retrieval_top_k, 4);
        assert_eq!(s.home, PathBuf::from(".joy"));
    }

    #[test]
    fn reads_joy_provider_from_env() {
        std::env::set_var("JOY_PROVIDER", "deepseek");
        let s = Settings::from_env();
        assert_eq!(s.provider, "deepseek", "应当读到 JOY_PROVIDER");
        std::env::remove_var("JOY_PROVIDER");
    }

    #[test]
    fn empty_env_value_is_treated_as_unset() {
        // .env 里常见 `JOY_MODEL=` 这种留空行 —— 应当视为没设，
        // 让 provider 的默认值生效，而不是把空串当成模型名发出去。
        std::env::set_var("JOY_MODEL", "");
        let s = Settings::from_env();
        assert_eq!(s.model, None);
        std::env::remove_var("JOY_MODEL");
    }

    #[test]
    fn bool_accepts_the_three_spellings() {
        for (v, expected) in [
            ("1", true),
            ("true", true),
            ("yes", true),
            ("0", false),
            ("", false),
        ] {
            let key = "JOY_CONFIG_TEST_BOOL_XYZ";
            std::env::set_var(key, v);
            assert_eq!(env_bool(key), expected, "{key}={v}");
            std::env::remove_var(key);
        }
    }

    #[test]
    fn view_is_complete() {
        let s = Settings::default();
        let v = s.view();
        assert_eq!(v.provider, "anthropic");
        assert_eq!(v.max_tokens, 8192);
    }

    #[test]
    fn apply_patch_overlays_and_clears_empty_overrides() {
        let mut s = Settings::default();
        let patch = joyczl_protocol::SettingsPatch {
            provider: Some("deepseek".to_string()),
            // 空串 = 清掉显式覆盖，回到 provider 默认。
            model: Some("   ".to_string()),
            max_tokens: Some(4096),
            graph_workflows: Some(true),
            ..Default::default()
        };
        apply_patch(&patch, &mut s);
        assert_eq!(s.provider, "deepseek");
        assert_eq!(s.model, None);
        assert_eq!(s.max_tokens, 4096);
        assert!(s.graph_workflows);
    }

    #[test]
    fn merge_newer_keeps_newer_values_and_keeps_older_ones_otherwise() {
        let mut base = joyczl_protocol::SettingsPatch {
            max_iterations: Some(5),
            provider: Some("anthropic".to_string()),
            ..Default::default()
        };
        let newer = joyczl_protocol::SettingsPatch {
            max_iterations: Some(9),
            graph_workflows: Some(true),
            ..Default::default()
        };
        base.merge_newer(&newer);
        assert_eq!(base.max_iterations, Some(9));
        assert_eq!(base.provider.as_deref(), Some("anthropic"));
        assert_eq!(base.graph_workflows, Some(true));
    }

    #[test]
    fn patch_round_trips_through_disk() {
        let dir = tempfile::tempdir().expect("临时目录");
        let patch = joyczl_protocol::SettingsPatch {
            max_iterations: Some(20),
            ..Default::default()
        };
        save_patch(dir.path(), &patch).expect("落盘");
        let loaded = load_patch(dir.path());
        assert_eq!(loaded.max_iterations, Some(20));
    }

    #[test]
    fn saving_an_empty_patch_removes_the_file() {
        let dir = tempfile::tempdir().expect("临时目录");
        save_patch(
            dir.path(),
            &joyczl_protocol::SettingsPatch {
                max_iterations: Some(20),
                ..Default::default()
            },
        )
        .expect("先落一份");
        save_patch(dir.path(), &joyczl_protocol::SettingsPatch::default()).expect("再清掉");
        assert!(!patch_path(dir.path()).exists(), "空补丁不该留文件");
    }

    #[test]
    fn a_corrupt_patch_file_reads_as_no_patch() {
        let dir = tempfile::tempdir().expect("临时目录");
        std::fs::write(patch_path(dir.path()), "这不是 JSON").expect("写坏文件");
        assert!(load_patch(dir.path()).is_empty());
        // 文件不存在也一样。
        assert!(load_patch(std::path::Path::new("/definitely/not/here")).is_empty());
    }

    // ---- 启动期校验 ---------------------------------------------------------

    #[test]
    fn the_defaults_pass_validation() {
        assert!(Settings::default().validate().is_ok());
    }

    #[test]
    fn an_out_of_range_number_names_the_env_var() {
        // 报错必须指出**哪个变量**错了，否则用户要在二十几个旋钮里猜。
        let s = Settings {
            history_turns: -5,
            ..Settings::default()
        };
        let error = s.validate().expect_err("负数窗口要被抓到");
        assert!(error.contains("JOY_HISTORY_TURNS"), "{error}");
        assert!(error.contains("-5"), "要说清收到的值：{error}");

        let s = Settings {
            exec_timeout_secs: 0,
            ..Settings::default()
        };
        assert!(s
            .validate()
            .expect_err("0 秒超时没意义")
            .contains("JOY_EXEC_TIMEOUT"));

        let s = Settings {
            max_tokens: 4,
            ..Settings::default()
        };
        assert!(s
            .validate()
            .expect_err("4 个 token 什么都答不出来")
            .contains("JOY_MAX_TOKENS"));
    }

    #[test]
    fn broken_exec_allow_rules_are_caught_at_startup() {
        // 空串：不是一条规则（`from_env` 会过滤掉，但补丁路径塞得进来）。
        let s = Settings {
            exec_allow: vec!["cargo test".to_string(), "   ".to_string()],
            ..Settings::default()
        };
        assert!(s.validate().expect_err("空规则要被抓到").contains("空规则"));

        // 换行：多半是把整个脚本贴进了环境变量。
        let s = Settings {
            exec_allow: vec!["cargo test\nrm -rf /".to_string()],
            ..Settings::default()
        };
        assert!(s.validate().expect_err("换行规则要被抓到").contains("换行"));

        // 超长。
        let s = Settings {
            exec_allow: vec!["x".repeat(300)],
            ..Settings::default()
        };
        assert!(s.validate().expect_err("超长规则要被抓到").contains("最长"));

        // 条数。
        let s = Settings {
            exec_allow: vec!["ls".to_string(); 100],
            ..Settings::default()
        };
        assert!(s.validate().expect_err("规则太多要被抓到").contains("最多"));

        // 空表合法：什么都不放行是**默认**，不是错误。
        let s = Settings {
            exec_allow: Vec::new(),
            ..Settings::default()
        };
        assert!(s.validate().is_ok());
    }

    #[test]
    fn the_compaction_threshold_is_a_ratio() {
        // 0 会让每一轮都压缩，1 以上会让压缩永远不发生 —— 两者都是配错了。
        for bad in [0.0, 0.01, 1.0, 2.0, -0.5] {
            let s = Settings {
                compact_threshold: bad,
                ..Settings::default()
            };
            let error = s.validate().expect_err("越界的比例要被抓到");
            assert!(error.contains("JOY_COMPACT_THRESHOLD"), "{error}");
        }
        let s = Settings {
            compact_threshold: 0.5,
            ..Settings::default()
        };
        assert!(s.validate().is_ok());
    }

    #[test]
    fn a_window_override_must_be_sane() {
        let too_small = Settings {
            context_window: Some(10),
            ..Settings::default()
        };
        assert!(too_small
            .validate()
            .expect_err("10 的窗口没意义")
            .contains("JOY_CONTEXT_WINDOW"));

        // 答案额度比窗口还大：永远答不完，属于配错了。
        let impossible = Settings {
            context_window: Some(4096),
            max_tokens: 8192,
            ..Settings::default()
        };
        assert!(impossible
            .validate()
            .expect_err("答案放不下")
            .contains("JOY_MAX_TOKENS"));

        let ok = Settings {
            context_window: Some(8192),
            max_tokens: 1024,
            ..Settings::default()
        };
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn writable_roots_must_exist_and_be_absolute() {
        let dir = tempfile::tempdir().expect("临时目录");
        let ok = Settings {
            exec_writable_roots: vec![dir.path().to_path_buf()],
            ..Settings::default()
        };
        assert!(ok.validate().is_ok());

        let missing = Settings {
            exec_writable_roots: vec![PathBuf::from("/definitely/not/here")],
            ..Settings::default()
        };
        assert!(missing
            .validate()
            .expect_err("不存在的目录要被抓到")
            .contains("不存在"));

        let relative = Settings {
            exec_writable_roots: vec![PathBuf::from("build")],
            ..Settings::default()
        };
        assert!(relative
            .validate()
            .expect_err("相对路径没法塞进沙箱规则")
            .contains("绝对路径"));
    }

    #[test]
    fn the_sandbox_is_offline_unless_asked_otherwise() {
        // 默认值本身就是要被钉住的行为：不开这个开关，命令不能联网。
        assert!(!Settings::default().exec_network);
        assert!(Settings::default().exec_writable_roots.is_empty());
    }

    #[test]
    fn patch_validation_reuses_the_same_bounds() {
        // 补丁路径与启动期共用 BOUNDS：同样的越界，同样的数字。
        use joyczl_protocol::SettingsPatch;
        let bad = SettingsPatch {
            max_tokens: Some(1),
            ..Default::default()
        };
        let error = validate_patch_values(&bad).expect_err("越界补丁要拒");
        assert!(error.contains("maxTokens"), "{error}");
        assert!(error.contains("128"), "要说清下界：{error}");

        let ok = SettingsPatch {
            max_tokens: Some(4096),
            history_turns: Some(0),
            ..Default::default()
        };
        assert!(validate_patch_values(&ok).is_ok());
    }
}
