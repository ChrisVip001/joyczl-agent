//! `joyczl-config` — every knob is an environment variable; see the root `.env.example`.
//!
//! Note Joy does **not** read the `.env` file: that template is for humans to source
//! (`set -a; source .env; set +a`), not loaded automatically.
//!
//! No settings framework: read once at startup, then it's an immutable plain struct.
//! If you can read this file, you know what Joy can be configured into.

pub mod models;
pub mod routes;
pub mod workspaces;

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

// ---- The knobs' bounds: single source of truth ----------------------------------------------
//
// Two entry points share this table: **startup** (`Settings::validate`, exits on the
// spot for illegal values) and **config/write** (`validate_patch_values`, rejects the
// whole patch). Before, only the latter validated, so `JOY_HISTORY_TURNS=-5` or
// `JOY_EXEC_TIMEOUT=abc` would silently become defaults or zero windows at startup.
//
// The table carries both patch field names and env var names; errors pick one by source.

pub struct Bound {
    /// Field name in `SettingsPatch` (used by the `config/write` path).
    /// `None` = settable only via env vars (currently the two timeouts).
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
        env_name: "JOY_TOOL_RESULT_TOTAL_CHARS",
        min: 0,
        max: 4_000_000,
    },
    Bound {
        patch_name: None,
        env_name: "JOY_TOOL_RESULT_MAX_CHARS",
        min: 0,
        max: 1_000_000,
    },
    Bound {
        patch_name: None,
        env_name: "JOY_APPROVAL_TIMEOUT",
        min: 5,
        max: 3_600,
    },
    Bound {
        patch_name: None,
        env_name: "JOY_GOAL_MAX_ROUNDS",
        min: 1,
        max: 20,
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
        env_name: "JOY_HUB_MAX_OPEN",
        min: 1,
        max: 4_096,
    },
    Bound {
        env_name: "JOY_HUB_IDLE_SECS",
        patch_name: None,
        min: 0,
        max: 86_400,
    },
    Bound {
        patch_name: None,
        env_name: "JOY_HOOKS_TIMEOUT",
        min: 1,
        max: 300,
    },
    Bound {
        patch_name: None,
        env_name: "JOY_EXEC_TIMEOUT",
        min: 1,
        max: 3_600,
    },
];

/// Count and per-entry length caps for allow rules. Rules are handwritten; hundreds of
/// entries or thousands of chars can only be an accidental paste (e.g. a whole script).
const MAX_EXEC_RULES: usize = 64;
const MAX_EXEC_RULE_CHARS: usize = 200;

/// Look up a bound by patch field name or env var name and error out.
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

// PartialEq only, no Eq: `compact_threshold` is f64, and f64 is not Eq
// (NaN makes "equal" non-reflexive). Everything else is integers and bools —
// no point pretending Eq is usable where it is not.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    // ---- LLM: pick a provider and configure its key. See PROVIDERS in joyczl-provider.
    pub provider: String,
    /// Explicit overrides: key, endpoint, model id. Empty = provider's own defaults.
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    /// Cheap model: used by the retrieval gate and consolidation.
    pub small_model: Option<String>,

    // ---- Home: all of Joy's state lives here (state.db, SOUL.md, traces/).
    pub home: PathBuf,

    // ---- Loop guardrails
    /// `JOY_GOAL_MAX_ROUNDS`: max auto-continuation rounds per goal (default 5). Beyond
    /// that, honestly set `round-limit` — no faking completion, no clearing the goal.
    pub goal_max_rounds: i32,
    /// `JOY_LLM_RETRIES`: max backoff retries on rate limits / transient faults (default
    /// 2, cap 5, 0 = off). Only retries errors where "try again" makes sense, and
    /// **always sends a notification per retry** — see retry.rs in joyczl-provider.
    pub llm_retries: i32,
    /// Timeout for a single model call. A hung network call must never silently freeze a turn.
    pub llm_timeout_secs: i64,
    pub max_iterations: i32,
    /// Leave headroom: reasoning models (kimi-k3, gpt-5.x, gemini-*-pro) spend output tokens
    /// thinking first; a low cap leaves stop_reason=max_tokens stuck mid-thought.
    pub max_tokens: i32,
    /// Working-memory sliding window: only the last N turns go into the prompt. Older
    /// ones stay in state.db, recovered via the retrieval gate + episodic memory.
    ///
    /// It is an **upper bound**, not a trigger: tokens decide when compaction happens,
    /// see `compact_threshold`.
    pub history_turns: i32,
    /// `JOY_CONTEXT_WINDOW`: overrides the approximate context window in the provider
    /// table. Local models (ollama / LM Studio) vary wildly; the table holds common defaults.
    pub context_window: Option<u32>,
    /// `JOY_COMPACT_THRESHOLD`: fraction of the context window used before compacting
    /// (default 0.8). Turns are the cap, tokens are the gate — one long tool output can
    /// blow the window while the turn count still looks "safe".
    pub compact_threshold: f64,

    // ---- Memory
    /// Distill facts only once every N turns of new conversation.
    pub consolidate_every: i32,
    pub retrieval_top_k: i32,

    // ---- Cockpit HTTP surface (see joyczl-ops)
    /// `JOY_OPS_HOST`: address the cockpit binds to. Default 127.0.0.1 — a personal assistant
    /// isn't exposed by default; viewing remotely is **another decision**: set `JOY_OPS_TOKEN` too.
    pub ops_host: String,
    /// `JOY_OPS_TOKEN`: Bearer token for the cockpit's `/api/*` (constant-time compare).
    /// Binding a non-loopback address without it = rejected at startup, not a warning. It is
    /// **deliberately not in `view()`**: a deployment param the model must not see in payloads.
    pub ops_token: Option<String>,

    // ---- Switches
    pub apple_calendar: bool,
    pub google_calendar: bool,
    pub experimental: bool,
    pub graph_workflows: bool,

    // ---- Hybrid retrieval (the vector leg; see embed.rs in joyczl-provider)
    /// `JOY_EMBEDDINGS=1`: blend vector similarity with keyword results at retrieval time.
    /// Off by default — it needs an embedding model to matter, plus one extra network call.
    pub embeddings_enabled: bool,
    /// `JOY_EMBED_MODEL`: embedding model name (e.g. nomic-embed-text).
    pub embed_model: Option<String>,

    // ---- In-turn tool result budget (see budget.rs in joyczl-loop)
    /// `JOY_TOOL_RESULT_TOTAL_CHARS`: total char cap for all tool results within a turn.
    /// When exceeded, replace from the largest with stubs (full text spilled to disk +
    /// head/tail previews). Default 200000 — only pathological output hits it; 0 disables.
    pub tool_result_total_chars: i64,
    /// `JOY_TOOL_RESULT_MAX_CHARS`: a result must exceed this to qualify for stubbing.
    /// Default 30000 (about 8k tokens). Set 0 to disable.
    pub tool_result_max_chars: i64,

    // ---- Lifecycle hooks (see hooks.rs in joyczl-tools)
    /// `JOY_HOOKS`: only when enabled does Joy read `<home>/hooks.json` and run its commands.
    /// Off by default — external commands on every tool call is a switch pressed by hand.
    pub hooks_enabled: bool,
    /// `JOY_HOOKS_TIMEOUT`: default hook timeout (seconds). An entry's `timeout` wins.
    pub hooks_timeout_secs: i64,

    /// `JOY_HUB_MAX_OPEN`: max user app-servers the hub keeps open at once (each is a
    /// subprocess). When full, evict "least recently used and idle" — three policies:
    /// a cap, LRU, and in-use instances are never evicted.
    pub hub_max_open: i64,

    /// `JOY_HUB_IDLE_SECS`: how long a user's app-server may idle before closing
    /// (seconds). 0 = no idle eviction, only the cap applies. Each instance is a
    /// subprocess; closing idle ones is the baseline of resource density.
    pub hub_idle_secs: i64,

    /// `JOY_NOTIFY`: only when enabled does Joy read `<home>/notify.json` and push
    /// notifications to its webhooks (approval requests / job completion / blocked goals).
    /// Off by default — sending data to external services is a decision pressed by hand.
    pub notify_enabled: bool,

    // ---- Execution (`run_command`; see exec.rs in joyczl-tools)
    /// `JOY_DELEGATE`: only when enabled is `delegate_task` registered in the tool table.
    /// Off by default — "hand the work off and let it run" is capability as well as risk
    /// (one more context, one more unread trail); the switch too must be pressed by hand.
    pub delegate_enabled: bool,
    /// `JOY_EXEC`: only when enabled is run_command registered in the tool table. Off
    /// by default — an assistant that can execute commands must be switched on by hand.
    pub exec_enabled: bool,
    /// `JOY_EXEC_ALLOW`: allow rules (comma-separated; trailing `*` wildcard supported).
    /// **Empty list = nothing is allowed**: deny by default, not allow by default.
    pub exec_allow: Vec<String>,
    /// `JOY_EXEC_TIMEOUT`: timeout for a single command (seconds).
    pub exec_timeout_secs: i64,

    // ---- Interactive approval (see approval.rs in joyczl-tools)
    /// `JOY_APPROVAL`: what happens when nothing matches the allow table.
    ///
    /// * `never` (default) — reject outright (same as the old behavior).
    /// * `on-request` — ask once, then decide; **no answer counts as rejection**.
    ///
    /// The hard-deny list and the sandbox are **never negotiable**: approval only opens the gate.
    pub approval: String,
    /// `JOY_APPROVAL_TIMEOUT`: seconds to wait for an answer (default 120).
    pub approval_timeout_secs: i64,
    /// `JOY_EXEC_NETWORK`: whether the sandbox **may reach the network**. Default **false
    /// (offline)** — an allowed command should not by default own the ability to ship data
    /// out. Commands like `cargo test` needing downloads require explicitly setting it to 1.
    pub exec_network: bool,
    /// `JOY_EXEC_WRITABLE_ROOTS`: extra writable dirs beyond workdir and home (colon-separated
    /// absolute paths, must already exist). For build caches like `cargo build`.
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
            goal_max_rounds: 5,
            llm_retries: 2,
            llm_timeout_secs: 120,
            max_iterations: 10,
            max_tokens: 8192,
            history_turns: 12,
            context_window: None,
            compact_threshold: 0.8,
            consolidate_every: 6,
            retrieval_top_k: 4,
            ops_host: "127.0.0.1".to_string(),
            ops_token: None,
            notify_enabled: false,
            apple_calendar: false,
            google_calendar: false,
            experimental: false,
            graph_workflows: false,
            embeddings_enabled: false,
            embed_model: None,
            delegate_enabled: false,
            tool_result_total_chars: 200_000,
            tool_result_max_chars: 30_000,
            hooks_enabled: false,
            hub_max_open: 32,
            hub_idle_secs: 900,
            hooks_timeout_secs: 30,
            exec_enabled: false,
            exec_allow: Vec::new(),
            exec_timeout_secs: 30,
            approval: "never".to_string(),
            approval_timeout_secs: 120,
            exec_network: false,
            exec_writable_roots: Vec::new(),
        }
    }
}

impl Settings {
    /// Read from env vars — the only read path for runtime parameters; no config file, no rules.
    ///
    /// **The exception is models and keys**: they are not configured via env vars but
    /// via `<home>/models.json` + the system keychain (see models.rs). Only `provider`
    /// is kept here as the fallback vendor when nothing is configured, so local models
    /// like ollama can start zero-config; `model` / `base_url` / `api_key` stay empty,
    /// to be filled by `models::apply_active`.
    pub fn from_env() -> Self {
        let d = Settings::default();
        Self {
            provider: env("JOY_PROVIDER").unwrap_or(d.provider),
            // Filled by the active entry in <home>/models.json, not read from env vars.
            api_key: None,
            base_url: None,
            model: None,
            small_model: None,
            home: env("JOY_HOME").map(PathBuf::from).unwrap_or(d.home),
            goal_max_rounds: env_int("JOY_GOAL_MAX_ROUNDS", d.goal_max_rounds),
            llm_retries: env_int("JOY_LLM_RETRIES", d.llm_retries),
            llm_timeout_secs: env_int("JOY_LLM_TIMEOUT", d.llm_timeout_secs as i32) as i64,
            max_iterations: env_int("JOY_MAX_ITERATIONS", d.max_iterations),
            max_tokens: env_int("JOY_MAX_TOKENS", d.max_tokens),
            history_turns: env_int("JOY_HISTORY_TURNS", d.history_turns),
            context_window: env("JOY_CONTEXT_WINDOW").and_then(|v| v.parse().ok()),
            compact_threshold: env_float("JOY_COMPACT_THRESHOLD", d.compact_threshold),
            consolidate_every: env_int("JOY_CONSOLIDATE_EVERY", d.consolidate_every),
            retrieval_top_k: env_int("JOY_RETRIEVAL_TOP_K", d.retrieval_top_k),
            ops_host: env("JOY_OPS_HOST").unwrap_or(d.ops_host),
            ops_token: env("JOY_OPS_TOKEN"),
            notify_enabled: env_bool("JOY_NOTIFY"),
            apple_calendar: env_bool("JOY_APPLE_CALENDAR"),
            google_calendar: env_bool("JOY_GOOGLE_CALENDAR"),
            experimental: env_bool("JOY_EXPERIMENTAL"),
            graph_workflows: env_bool("JOY_GRAPH_WORKFLOWS"),
            embeddings_enabled: env_bool("JOY_EMBEDDINGS"),
            embed_model: env("JOY_EMBED_MODEL"),
            delegate_enabled: env_bool("JOY_DELEGATE"),
            tool_result_total_chars: env_int(
                "JOY_TOOL_RESULT_TOTAL_CHARS",
                d.tool_result_total_chars as i32,
            ) as i64,
            tool_result_max_chars: env_int(
                "JOY_TOOL_RESULT_MAX_CHARS",
                d.tool_result_max_chars as i32,
            ) as i64,
            hooks_enabled: env_bool("JOY_HOOKS"),
            hub_max_open: env_int("JOY_HUB_MAX_OPEN", d.hub_max_open as i32) as i64,
            hub_idle_secs: env_int("JOY_HUB_IDLE_SECS", d.hub_idle_secs as i32) as i64,
            hooks_timeout_secs: env_int("JOY_HOOKS_TIMEOUT", d.hooks_timeout_secs as i32) as i64,
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
            approval: env("JOY_APPROVAL").unwrap_or(d.approval),
            approval_timeout_secs: env_int("JOY_APPROVAL_TIMEOUT", d.approval_timeout_secs as i32)
                as i64,
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

    /// Ensure home and its subdirectories exist.
    pub fn ensure_home(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.home.join("traces"))?;
        std::fs::create_dir_all(self.home.join("outbox"))?;
        std::fs::create_dir_all(self.home.join("skills"))?;
        // Where overlong command output is spilled to disk (see joyczl-tools exec.rs, limitations).
        std::fs::create_dir_all(self.home.join("spill"))?;
        Ok(())
    }

    /// Startup validation: illegal values **error out on the spot**, never silently default.
    ///
    /// The caller (`app-server`'s `open`) exits on `Err` and prints the reason — "failing to
    /// start" is far cheaper and far easier to debug than "runs but misbehaves".
    pub fn validate(&self) -> Result<(), String> {
        check_bound("JOY_MAX_ITERATIONS", self.max_iterations as i64)?;
        check_bound("JOY_MAX_TOKENS", self.max_tokens as i64)?;
        check_bound("JOY_HISTORY_TURNS", self.history_turns as i64)?;
        check_bound("JOY_CONSOLIDATE_EVERY", self.consolidate_every as i64)?;
        check_bound("JOY_RETRIEVAL_TOP_K", self.retrieval_top_k as i64)?;
        check_bound("JOY_GOAL_MAX_ROUNDS", self.goal_max_rounds as i64)?;
        check_bound("JOY_LLM_RETRIES", self.llm_retries as i64)?;
        check_bound("JOY_LLM_TIMEOUT", self.llm_timeout_secs)?;
        check_bound("JOY_HUB_MAX_OPEN", self.hub_max_open)?;
        check_bound("JOY_HUB_IDLE_SECS", self.hub_idle_secs)?;
        check_bound("JOY_HOOKS_TIMEOUT", self.hooks_timeout_secs)?;
        check_bound("JOY_EXEC_TIMEOUT", self.exec_timeout_secs)?;
        check_bound("JOY_APPROVAL_TIMEOUT", self.approval_timeout_secs)?;
        check_bound("JOY_TOOL_RESULT_TOTAL_CHARS", self.tool_result_total_chars)?;
        check_bound("JOY_TOOL_RESULT_MAX_CHARS", self.tool_result_max_chars)?;
        if !matches!(self.approval.as_str(), "never" | "on-request") {
            return Err(format!(
                "JOY_APPROVAL 只能是 never 或 on-request，收到 '{}'",
                self.approval
            ));
        }

        // While the window is overridden, check one relation: the answer's budget can't
        // exceed the window — the model never finishes, showing up as "mysteriously truncated".
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

        // Ratio-type knobs validated separately (not integers, so they cannot enter BOUNDS).
        if !(0.05..=0.95).contains(&self.compact_threshold) {
            return Err(format!(
                "JOY_COMPACT_THRESHOLD 应该在 0.05 到 0.95 之间，收到 {}",
                self.compact_threshold
            ));
        }

        // Extra writable roots: must be **existing absolute dirs**. When writing sandbox
        // rules they get embedded there; a nonexistent path just makes the rule silently
        // fail (seatbelt rejects nonexistent subpaths) — "I allowed it but writes still fail".
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

        check_exec_rules(&self.exec_allow)?;
        Ok(())
    }

    /// The `config/read` payload: the readback must be the full current state; fields are total.
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

// ---- config/write: patch merging, persistence and application -------------------------------
//
// Env vars remain the single source of the knobs (`from_env`), unchanged. The dashboard's
// `config/write` needs changes to survive restarts, so there is one more **patch** layer:
// a `SettingsPatch` written to `<home>/settings.json`, overlaid on env values at startup.
// Absent = left to the env; present = explicit override. Who spoke last is plain in the file.

/// `<home>/settings.json` — where `config/write` persists.
pub fn patch_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join("settings.json")
}

/// **Numeric** validation for `config/write` patches: shares `BOUNDS` with startup.
///
/// Provider existence is not checked here — `joyczl-config` does not know `PROVIDERS`
/// (that is the provider layer's business); the caller (app-server) adds that step.
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
    // Allow-list replaced as a whole ("remember this command" uses it): same rules as startup.
    if let Some(rules) = &patch.exec_allow {
        check_exec_rules(rules)?;
    }
    Ok(())
}

/// Allow rules: an empty list is legal (= nothing allowed; that is the default). But every
/// entry must be a **working** rule — a broken rule silently never matches, yet still shows
/// up in rejection reasons, which is painful to debug. Shared by startup and `config/write`.
fn check_exec_rules(rules: &[String]) -> Result<(), String> {
    if rules.len() > MAX_EXEC_RULES {
        return Err(format!(
            "JOY_EXEC_ALLOW 最多 {MAX_EXEC_RULES} 条规则，收到 {} 条",
            rules.len()
        ));
    }
    for rule in rules {
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

/// Read the saved patch. A missing file or a parse failure both count as "no patch":
/// a broken config file must not keep Joy from starting — env vars still backstop.
pub fn load_patch(home: &std::path::Path) -> joyczl_protocol::SettingsPatch {
    std::fs::read_to_string(patch_path(home))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Write the patch to `<home>/settings.json`. An all-empty patch has nothing to say — delete
/// the file, so after `config/write` resets all fields, "no overrides in this dir" holds.
pub fn save_patch(
    home: &std::path::Path,
    patch: &joyczl_protocol::SettingsPatch,
) -> std::io::Result<()> {
    let path = patch_path(home);
    if patch.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => return Ok(()),
            // Didn't exist in the first place: the goal is already achieved.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
    }
    let raw = serde_json::to_string_pretty(patch).expect("SettingsPatch 序列化");
    std::fs::write(&path, raw)
}

/// Overlay the patch onto settings. Only `Some` fields in the patch get a say.
///
/// **Models and keys are not in this patch**: they belong to `<home>/models.json`
/// (see models.rs); change them via `model/*` — one path beats two, and leaves no gap for
/// "keys casually written into settings.json".
pub fn apply_patch(patch: &joyczl_protocol::SettingsPatch, settings: &mut Settings) {
    let p = patch;
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
    if let Some(v) = &p.exec_allow {
        settings.exec_allow = v.clone();
    }
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
        // `.env` files commonly contain empty lines like `JOY_MODEL=` — treat as unset, so the
        // provider default applies instead of sending the empty string out as a model name.
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
    fn apply_patch_overlays_only_the_fields_it_carries() {
        let mut s = Settings::default();
        let patch = joyczl_protocol::SettingsPatch {
            max_tokens: Some(4096),
            graph_workflows: Some(true),
            ..Default::default()
        };
        apply_patch(&patch, &mut s);
        assert_eq!(s.max_tokens, 4096);
        assert!(s.graph_workflows);
        // Fields not carried are untouched.
        assert_eq!(s.max_iterations, Settings::default().max_iterations);
    }

    /// Models and keys are **not** in this patch — change them via `model/*`; keys go only into
    /// models.json / the keychain. This assertion pins "the patch has no such gaps".
    #[test]
    fn the_patch_has_no_model_or_key_fields_to_leak() {
        let patch = joyczl_protocol::SettingsPatch::default();
        let serialized = serde_json::to_string(&patch).expect("序列化");
        for leaked in ["provider", "model", "smallModel", "apiKey", "baseUrl"] {
            assert!(
                !serialized.contains(leaked),
                "补丁不该有模型/密钥字段：{serialized}"
            );
        }
    }

    #[test]
    fn merge_newer_keeps_newer_values_and_keeps_older_ones_otherwise() {
        let mut base = joyczl_protocol::SettingsPatch {
            max_iterations: Some(5),
            max_tokens: Some(1024),
            ..Default::default()
        };
        let newer = joyczl_protocol::SettingsPatch {
            max_iterations: Some(9),
            graph_workflows: Some(true),
            ..Default::default()
        };
        base.merge_newer(&newer);
        assert_eq!(base.max_iterations, Some(9));
        assert_eq!(base.max_tokens, Some(1024), "新补丁没带的字段要留着");
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
        // A missing file is the same.
        assert!(load_patch(std::path::Path::new("/definitely/not/here")).is_empty());
    }

    // ---- Startup validation ---------------------------------------------------------

    #[test]
    fn the_defaults_pass_validation() {
        assert!(Settings::default().validate().is_ok());
    }

    #[test]
    fn an_out_of_range_number_names_the_env_var() {
        // The error must name **which variable** is wrong, or users guess among two dozen knobs.
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
        // Empty string: not a rule (`from_env` filters it, but the patch path can inject it).
        let s = Settings {
            exec_allow: vec!["cargo test".to_string(), "   ".to_string()],
            ..Settings::default()
        };
        assert!(s.validate().expect_err("空规则要被抓到").contains("空规则"));

        // Newline: most likely a whole script pasted into an env var.
        let s = Settings {
            exec_allow: vec!["cargo test\nrm -rf /".to_string()],
            ..Settings::default()
        };
        assert!(s.validate().expect_err("换行规则要被抓到").contains("换行"));

        // Overlong.
        let s = Settings {
            exec_allow: vec!["x".repeat(300)],
            ..Settings::default()
        };
        assert!(s.validate().expect_err("超长规则要被抓到").contains("最长"));

        // Too many entries.
        let s = Settings {
            exec_allow: vec!["ls".to_string(); 100],
            ..Settings::default()
        };
        assert!(s.validate().expect_err("规则太多要被抓到").contains("最多"));

        // An empty list is legal: allowing nothing is the **default**, not an error.
        let s = Settings {
            exec_allow: Vec::new(),
            ..Settings::default()
        };
        assert!(s.validate().is_ok());
    }

    #[test]
    fn the_compaction_threshold_is_a_ratio() {
        // 0 would compact every turn; >=1 would make compaction never happen — both misconfig.
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

        // Answer budget larger than the window: it never finishes — a misconfiguration.
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

        // "Nonexistent" must be tested with an absolute path **the platform itself accepts**:
        // `/x/y` is not absolute on Windows at all (no drive letter) and gets caught by the
        // rule above first — so the assertion would always fail there. Use a nonexistent name
        // under the temp dir; both platforms land on the "nonexistent" branch.
        let missing = Settings {
            exec_writable_roots: vec![std::env::temp_dir().join("joy-not-here-9f3a1")],
            ..Settings::default()
        };
        let why = missing.validate().expect_err("不存在的目录要被抓到");
        assert!(why.contains("不存在"), "要落在「不存在」这条分支上：{why}");

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
        // The default itself is behavior to pin: without the switch, commands cannot network.
        assert!(!Settings::default().exec_network);
        assert!(Settings::default().exec_writable_roots.is_empty());
    }

    #[test]
    fn patch_validation_reuses_the_same_bounds() {
        // The patch path shares BOUNDS with startup: same overflow, same numbers.
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
