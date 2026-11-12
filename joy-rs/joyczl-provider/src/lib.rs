//! `joyczl-provider` — model access.
//!
//! loop speaks only one dialect: the Anthropic Messages shape (system/messages/tools in,
//! content blocks out). Providers plug in via two paths:
//!
//!   * anthropic native format → Anthropic, Kimi/Moonshot, GLM/Z.ai, MiniMax
//!   * openai compatible format → OpenAI, Gemini, DeepSeek, OpenRouter, xAI ...
//!   * local inference → Ollama (same openai compatible format, no key needed)
//!
//! The entire difference between the two wire formats is those two conversion functions
//! in openai.rs — together they are only a few dozen lines.
//!
//! Differences from the Python version (deliberate):
//!   * no more `SimpleNamespace` fake objects — these are real types;
//!   * streaming is a separate path (`Provider::stream`): text is streamed to `on_text` as
//!     it arrives, and the fully assembled answer is still returned at the end, so loop
//!     shares one logic across both paths, and the dashboard's typewriter effect uses it.

pub mod anthropic;
pub mod embed;
pub mod error;
pub mod mock;
pub mod openai;
pub mod retry;
pub mod sse;
pub mod tokens;

pub use error::ProviderError;

use std::sync::Arc;
use std::time::Duration;

use joyczl_config::Settings;
use serde::{Deserialize, Serialize};

// ---- The one dialect loop speaks --------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

/// content block of Anthropic Messages. `tag = "type"` + snake_case makes the serialized
/// output byte-identical to Anthropic's wire format, so anthropic.rs barely converts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        /// Gemini thinking models attach `thought_signature` to tool_call; the next turn
        /// must pass it back verbatim or 400. Only Gemini over the openai wire sets it;
        /// deserialization defaults to None and serializing None emits nothing — the
        /// Anthropic wire never sees this field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extra: Option<serde_json::Value>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
    },
    /// An image handed to the model: raw bytes, base64, no path — the model sees pixels,
    /// not a filename. `media_type` is an IANA image type ("image/png", "image/jpeg", …).
    /// The Anthropic wire wraps this in a `source` object (see anthropic.rs); the OpenAI
    /// wire becomes a data URI in array content (see openai.rs).
    Image {
        media_type: String,
        data: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// Join the plain text of a message (ignoring tool call blocks).
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// The **declaration** of a tool the model may call (not the implementation — that is joyczl-tools).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// Tokens this call read **from** the provider's prompt cache (billed at a fraction of
    /// the input price). OpenAI spells it `prompt_tokens_details.cached_tokens`, Anthropic
    /// `cache_read_input_tokens`; both land here, so a turn's real cost is one subtraction.
    #[serde(default)]
    pub cache_read_input_tokens: i64,
    /// Tokens this call **wrote into** the cache (Anthropic only, billed at a premium).
    /// Zero elsewhere — the reason to record it is that a cache miss is not free either.
    #[serde(default)]
    pub cache_creation_input_tokens: i64,
}

#[derive(Debug, Clone)]
pub struct CreateRequest {
    pub model: String,
    /// The part of the prompt that does not change within a session — soul, model
    /// identity, the method paragraph. Kept apart from `system` because Anthropic needs an
    /// explicit `cache_control` boundary: a prefix marked once is read back at a fraction
    /// of the input price on every later turn. Other wires simply concatenate it: a prefix
    /// that never changes already hits their automatic cache.
    pub cache_prefix: Option<String>,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub max_tokens: i32,
}

impl CreateRequest {
    /// The system prompt as one string, prefix first — what wires without a cache
    /// boundary send.
    pub fn system_text(&self) -> Option<String> {
        match (&self.cache_prefix, &self.system) {
            (Some(prefix), Some(tail)) if !tail.is_empty() => Some(format!("{prefix}\n\n{tail}")),
            (Some(prefix), _) => Some(prefix.clone()),
            (None, tail) => tail.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CreateResponse {
    pub stop_reason: StopReason,
    pub usage: Usage,
    pub content: Vec<ContentBlock>,
}

impl CreateResponse {
    pub fn tool_uses(&self) -> Vec<(&str, &str, &serde_json::Value)> {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => Some((id.as_str(), name.as_str(), input)),
                _ => None,
            })
            .collect()
    }

    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// The outlet for token-by-token text. The dashboard's typewriter effect relies on it.
pub type TextSink = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// A provider only needs to do two things. Think twice before adding a trait method — a
/// smaller interface lets more providers plug in.
///
/// The return type is a manually boxed Future instead of RPITIT: the latter cannot be a
/// trait object, and app-server needs to box different providers into one `Box<dyn Provider>`.
/// The boxed form equals what `#[async_trait]` generates, minus the macro dependency.
pub trait Provider: Send + Sync {
    fn create(
        &self,
        request: CreateRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<CreateResponse, ProviderError>> + Send + '_>,
    >;

    /// Streaming version: calls `on_text` as soon as text arrives and returns the **fully assembled
    /// answer** at the end (reassembled tool calls and usage) — loop's logic is identical on both.
    ///
    /// The default falls back to non-streaming: providers without streaming still work, minus
    /// the token-by-token callback.
    fn stream(
        &self,
        request: CreateRequest,
        _on_text: TextSink,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<CreateResponse, ProviderError>> + Send + '_>,
    > {
        Box::pin(self.create(request))
    }
}

// ---- provider catalog -------------------------------------------------------

/// The two wire formats. Choosing one decides which HTTP client is used.
/// Lowercase in JSON — the catalog file uses the same words as `JOY_PROVIDER`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Wire {
    Anthropic,
    OpenAi,
}

pub struct ProviderInfo {
    pub id: &'static str,
    pub wire: Wire,
    /// Env var holding the key. The key itself never goes into config files — a bearer
    /// token in an mcp.json-style file pasted into a bug report is a leak.
    ///
    /// **Empty = local endpoint** (Ollama): it needs no key, so `resolve` does not look.
    pub key_env: &'static str,
    pub base_url: Option<&'static str>,
    /// Main model (used by loop)
    pub model: &'static str,
    /// Cheap model (used by retrieval gate / consolidation)
    pub small_model: &'static str,
    /// The **approximate** context window (tokens). One single use: computing "when to compact"
    /// (see `tokens.rs` and `JOY_COMPACT_THRESHOLD`).
    ///
    /// Underestimating only compacts earlier (one extra cheap call); overestimating overflows —
    /// so values are always conservative. The real cap is what the provider reports as an
    /// error: those are recognized (`ProviderError::ContextOverflow`) and retried after compaction.
    pub context_window: u32,
    /// Whether the provider's **default models** accept image input. Only gates the friendly
    /// refusal: a text-only provider that receives an image would get an API 400 (which still
    /// becomes `Error:` text), so this flag turns a cryptic remote error into a clear local one.
    /// Wrongly `false` costs a capability; wrongly `true` costs a round trip — be conservative.
    pub vision: bool,
}

impl ProviderInfo {
    /// Whether this provider needs a key. Local inference does not — which is exactly why
    /// it can run offline, at zero cost, without sending the conversation off this machine.
    pub fn needs_key(&self) -> bool {
        !self.key_env.is_empty()
    }
}

/// These defaults are just starting points; since models.json is the single configuration
/// path, the active entry there overrides them (`resolve_with`, below).
/// openrouter's default is a `:free` id — it runs at zero cost (rate-limited).
pub static PROVIDERS: &[ProviderInfo] = &[
    ProviderInfo {
        id: "anthropic",
        wire: Wire::Anthropic,
        key_env: "ANTHROPIC_API_KEY",
        base_url: None,
        model: "claude-sonnet-5",
        small_model: "claude-haiku-4-5-20251001",
        context_window: 200_000,
        vision: true,
    },
    ProviderInfo {
        id: "openai",
        wire: Wire::OpenAi,
        key_env: "OPENAI_API_KEY",
        base_url: None,
        model: "gpt-5.5",
        small_model: "gpt-4.1-mini",
        context_window: 200_000,
        vision: true,
    },
    ProviderInfo {
        id: "openrouter",
        wire: Wire::OpenAi,
        key_env: "OPENROUTER_API_KEY",
        base_url: Some("https://openrouter.ai/api/v1"),
        model: "nvidia/nemotron-3-super-120b-a12b:free",
        small_model: "google/gemma-4-26b-a4b-it:free",
        context_window: 128_000,
        vision: false,
    },
    ProviderInfo {
        id: "gemini",
        wire: Wire::OpenAi,
        key_env: "GEMINI_API_KEY",
        base_url: Some("https://generativelanguage.googleapis.com/v1beta/openai/"),
        model: "gemini-3.5-flash",
        small_model: "gemini-3.1-flash-lite",
        context_window: 1_000_000,
        vision: true,
    },
    ProviderInfo {
        id: "deepseek",
        wire: Wire::OpenAi,
        key_env: "DEEPSEEK_API_KEY",
        base_url: Some("https://api.deepseek.com"),
        model: "deepseek-v4-pro",
        small_model: "deepseek-v4-pro",
        context_window: 128_000,
        vision: false,
    },
    ProviderInfo {
        id: "minimax",
        wire: Wire::Anthropic,
        key_env: "MINIMAX_API_KEY",
        base_url: Some("https://api.minimaxi.com/anthropic"),
        model: "MiniMax-M3",
        small_model: "MiniMax-M2",
        context_window: 200_000,
        vision: false,
    },
    ProviderInfo {
        id: "kimi",
        wire: Wire::Anthropic,
        key_env: "MOONSHOT_API_KEY",
        base_url: Some("https://api.moonshot.ai/anthropic"),
        model: "kimi-k3",
        small_model: "kimi-k2.6",
        context_window: 200_000,
        vision: false,
    },
    ProviderInfo {
        id: "glm",
        wire: Wire::Anthropic,
        key_env: "ZHIPU_API_KEY",
        base_url: Some("https://api.z.ai/api/anthropic"),
        model: "glm-5.2",
        small_model: "glm-5-turbo",
        context_window: 200_000,
        vision: false,
    },
    ProviderInfo {
        id: "xai",
        wire: Wire::OpenAi,
        key_env: "XAI_API_KEY",
        base_url: Some("https://api.x.ai/v1"),
        model: "grok-4",
        small_model: "grok-4-fast",
        context_window: 256_000,
        vision: true,
    },
    ProviderInfo {
        id: "opencode_zen",
        wire: Wire::OpenAi,
        key_env: "OPENCODE_ZEN_API_KEY",
        base_url: Some("https://opencode.ai/zen/v1"),
        model: "deepseek-v4-flash-free",
        small_model: "deepseek-v4-flash-free",
        context_window: 128_000,
        vision: false,
    },
    ProviderInfo {
        id: "opencode_go",
        wire: Wire::OpenAi,
        key_env: "OPENCODE_GO_API_KEY",
        base_url: Some("https://opencode.ai/zen/go/v1"),
        model: "deepseek-v4-flash",
        small_model: "deepseek-v4-flash",
        context_window: 128_000,
        vision: false,
    },
    ProviderInfo {
        // Local inference: no key, no network needed. An empty `key_env` declares exactly that.
        // Ollama exposes an OpenAI-compatible endpoint (`/v1`), so the same wire is reused —
        // LM Studio / vLLM look the same; point `JOY_BASE_URL` at them.
        id: "ollama",
        wire: Wire::OpenAi,
        key_env: "",
        base_url: Some("http://127.0.0.1:11434/v1"),
        // First `ollama pull qwen3:8b` (main model) and `qwen3:4b` (the cheap model for the
        // retrieval gate / consolidation); override via models.json, not env vars.
        model: "qwen3:8b",
        small_model: "qwen3:4b",
        context_window: 32_768,
        vision: false,
    },
];

/// Where to get a key. Included in error messages, saving the user a search.
fn key_url(id: &str) -> Option<&'static str> {
    match id {
        "anthropic" => Some("https://console.anthropic.com/settings/keys"),
        "openai" => Some("https://platform.openai.com/api-keys"),
        "gemini" => Some("https://aistudio.google.com/apikey"),
        "deepseek" => Some("https://platform.deepseek.com/api_keys"),
        "openrouter" => Some("https://openrouter.ai/keys"),
        "kimi" => Some("https://platform.moonshot.ai/console/api-keys"),
        "glm" => Some("https://z.ai/manage-apikey/apikey-list"),
        "minimax" => Some("https://platform.minimaxi.com/user-center/basic-information"),
        "xai" => Some("https://console.x.ai"),
        "opencode_zen" | "opencode_go" => Some("https://opencode.ai/zen"),
        _ => None,
    }
}

pub fn lookup(id: &str) -> Option<&'static ProviderInfo> {
    PROVIDERS.iter().find(|p| p.id == id)
}

pub mod catalog;

#[cfg(test)]
#[path = "provider_tests.rs"]
mod provider_tests;

/// End-to-end streaming tests need a local HTTP server and real sockets, hence their own
/// module: slower than pure conversion tests, but they cover "parse → transport → callback".
#[cfg(test)]
#[path = "streaming_tests.rs"]
mod streaming_tests;

/// What remains after key / endpoint / model are resolved — app-server runs loop with it.
///
/// `Arc` instead of `Box`: graph nodes are 'static closures, so it must **own** a client
/// to outlive this turn (both triage paths, classify / quick_reply, need it).
#[derive(Clone)]
pub struct Resolved {
    pub provider_id: String,
    pub client: Arc<dyn Provider>,
    pub model: String,
    pub small_model: String,
    /// Whether this provider's models accept image input (from the catalog row).
    pub vision: bool,
    /// The approximate context window, taken from the catalog row this turn resolved
    /// from. Stored now (it used to live in the static table) because catalog rows are
    /// runtime data — the row may not exist in any 'static table.
    pub context_window: u32,
}

/// Context window for providers not in the table (mocks, evals, self-built gateways).
/// Conservative: compacting early only costs one cheap call; overflowing costs a whole round.
pub const DEFAULT_CONTEXT_WINDOW: u32 = 32_768;

impl Resolved {
    /// The approximate context window for this turn.
    pub fn context_window(&self) -> u32 {
        self.context_window
    }
}

// Handwritten Debug: dyn Provider cannot derive, but expect_err in tests needs it.
// The client has no state worth printing (the key must never hit logs), so it is omitted.
impl std::fmt::Debug for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolved")
            .field("provider_id", &self.provider_id)
            .field("model", &self.model)
            .field("small_model", &self.small_model)
            .finish_non_exhaustive()
    }
}

/// Resolve a usable client from Settings. On a missing key, give **actionable** guidance:
/// where to get a key, which line to add to which file, and which providers exist.
/// Resolve against the built-in table only. Kept for tests and callers that have no
/// home directory (embeddings); app-server resolves against the loaded catalog.
pub fn resolve(settings: &Settings) -> Result<Resolved, String> {
    resolve_with(settings, &catalog::Catalog::built_in())
}

/// Resolve a usable client from Settings and the **merged** catalog. On a missing key,
/// give **actionable** guidance: where to get a key, which line to add to which file,
/// and which providers exist.
pub fn resolve_with(settings: &Settings, catalog: &catalog::Catalog) -> Result<Resolved, String> {
    let info = catalog.lookup(&settings.provider).ok_or_else(|| {
        format!(
            "未知的 JOY_PROVIDER '{}'。可选：{}",
            settings.provider,
            catalog.ids().join(", ")
        )
    })?;

    // The key has **exactly one source**: the credential pointer of the active entry in
    // `<home>/models.json` (env var / keychain / fallback file), resolved by
    // `joyczl-config::models` into `settings.api_key`.
    //
    // Here we do **not** silently fall back to `info.key_env`: that implicit path makes
    // "I never configured a model, yet it ran because my shell had a variable" — the
    // hardest bug to trace. To use an env var, say so: `/model add <name> <vendor> --env VAR`.
    //
    // Local providers (`needs_key()` false) need no key: left empty, the client's `authed`
    // then sends no Authorization header.
    let api_key = settings
        .api_key
        .clone()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .or_else(|| (!info.needs_key()).then(String::new))
        .ok_or_else(|| no_key_message(&info.id, &info.key_env, &catalog.ids().join(", ")))?;

    let base_url = settings.base_url.clone().or_else(|| info.base_url.clone());
    let timeout = Duration::from_secs(settings.llm_timeout_secs.max(1) as u64);

    let max_retries = settings.llm_retries.max(0) as u32;
    let client: Arc<dyn Provider> = match info.wire {
        Wire::Anthropic => Arc::new(anthropic::Client::new(
            &api_key,
            base_url.as_deref(),
            timeout,
            max_retries,
        )),
        Wire::OpenAi => Arc::new(openai::Client::new(
            &api_key,
            base_url.as_deref(),
            timeout,
            max_retries,
        )),
    };

    Ok(Resolved {
        provider_id: info.id.clone(),
        client,
        model: settings.model.clone().unwrap_or_else(|| info.model.clone()),
        small_model: settings.small_model.clone().unwrap_or_else(|| {
            info.small_model
                .clone()
                .unwrap_or_else(|| info.model.clone())
        }),
        context_window: info.context_window,
        // The profile's own declaration wins: one vendor can ship a vision model and a
        // text-only one, and the catalog row only describes the vendor's **default** model.
        vision: settings.vision.unwrap_or(info.vision),
    })
}

fn no_key_message(id: &str, key_env: &str, known: &str) -> String {
    let mut msg = format!("provider '{id}' 没有 API key。\n");
    if let Some(url) = key_url(id) {
        msg.push_str(&format!("  1. 去领一个 key：{url}\n"));
    }
    msg.push_str(&format!(
        "  2. 配进 Joy（`--env {var}` 只读环境变量、不落盘；key 优先交给系统钥匙串）：\
         \n       /model add <名字> {id} --env {var}\n",
        var = key_env,
        id = id,
    ));
    msg.push_str(&format!(
        "  3. 或直接给 key（钥匙串不可用时会退回明文文件，界面会如实说是哪一种）：\
         \n       /model add <名字> {id} --key 你的-key\n",
        id = id,
    ));
    msg.push_str(&format!("其他 provider：{known}"));
    msg.push_str("\n切到另一套：/model use <名字>（配置在 REPL 与 <home>/models.json，驾驶舱不放模型面板）。");
    msg
}
