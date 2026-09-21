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

#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// 单次模型调用的超时。挂掉的网络调用绝不能让一轮 turn 静默冻结。
    pub llm_timeout_secs: i64,
    pub max_iterations: i32,
    /// 留够余量：推理模型（kimi-k3、gpt-5.x、gemini-*-pro）会先花输出 token
    /// 思考再作答，cap 太低会 stop_reason=max_tokens 卡在思考里。
    pub max_tokens: i32,
    /// 工作记忆滑窗：只把最近 N 轮塞进 prompt。更老的在 state.db 里，
    /// 靠检索门 + 情景记忆找回来。
    pub history_turns: i32,

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
            llm_timeout_secs: 120,
            max_iterations: 10,
            max_tokens: 8192,
            history_turns: 12,
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
            llm_timeout_secs: env_int("JOY_LLM_TIMEOUT", d.llm_timeout_secs as i32) as i64,
            max_iterations: env_int("JOY_MAX_ITERATIONS", d.max_iterations),
            max_tokens: env_int("JOY_MAX_TOKENS", d.max_tokens),
            history_turns: env_int("JOY_HISTORY_TURNS", d.history_turns),
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
        }
    }

    /// 确保 home 和它的子目录存在。
    pub fn ensure_home(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.home.join("traces"))?;
        std::fs::create_dir_all(self.home.join("outbox"))?;
        std::fs::create_dir_all(self.home.join("skills"))?;
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
}
