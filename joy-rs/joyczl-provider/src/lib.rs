//! `joyczl-provider` —— 模型接入。
//!
//! loop 只说一种方言：Anthropic Messages 的形状（system/messages/tools 进，
//! content blocks 出）。provider 用两种方式接进来：
//!
//!   * anthropic 原生格式 → Anthropic、Kimi/Moonshot、GLM/Z.ai、MiniMax
//!   * openai 兼容格式   → OpenAI、Gemini、DeepSeek、OpenRouter、xAI …
//!   * 本地推理           → Ollama（同一份 openai 兼容格式，不需要 key）
//!
//! 两种 wire format 之间的全部差异，就是 openai.rs 里那两个转换函数 ——
//! 两个转换函数加起来不过几十行。
//!
//! 与 Python 版的差异（刻意为之）：
//!   * 不再需要 `SimpleNamespace` 伪造对象 —— 这里是真的类型；
//!   * 流式是一条独立路径（`Provider::stream`）：文本边到边回调 `on_text`，
//!     结束时仍然返回组装好的完整应答，所以 loop 两条路径共用同一套逻辑，
//!     dashboard 的打字机效果直接用它。

pub mod anthropic;
pub mod embed;
pub mod error;
pub mod mock;
pub mod openai;
pub mod sse;
pub mod tokens;

pub use error::ProviderError;

use std::sync::Arc;
use std::time::Duration;

use joyczl_config::Settings;
use serde::{Deserialize, Serialize};

// ---- loop 说的一种方言 -----------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

/// Anthropic Messages 的 content block。`tag = "type"` + snake_case 让序列化
/// 结果与 Anthropic 线上格式逐字相同，所以 anthropic.rs 几乎不用做转换。
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
        /// Gemini 的思考模型在 tool_call 上附带 `thought_signature`，下一轮
        /// 必须原样回传否则 400。只有走 openai wire 的 Gemini 会设置它；
        /// 反序列化缺省 None、序列化 None 不输出 —— Anthropic 那条 wire
        /// 完全看不到这个字段。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extra: Option<serde_json::Value>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
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

    /// 把一条消息里的纯文本拼出来（忽略 tool 调用块）。
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

/// 模型能调的工具的**声明**（不是实现 —— 实现在 joyczl-tools）。
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
}

#[derive(Debug, Clone)]
pub struct CreateRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub max_tokens: i32,
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

/// 逐 token 文本的出口。dashboard 的打字机效果靠它。
pub type TextSink = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// provider 只需要会两件事。加一个 trait 方法就要想清楚 ——
/// 接口面越小，能接进来的 provider 越多。
///
/// 返回类型写成手动装箱的 Future 而不是 RPITIT：后者不能做 trait object，
/// 而 app-server 需要把不同 provider 装进同一个 `Box<dyn Provider>`。
/// 装箱写法等价于 `#[async_trait]` 生成的东西，只是没有那个宏的依赖。
pub trait Provider: Send + Sync {
    fn create(
        &self,
        request: CreateRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<CreateResponse, ProviderError>> + Send + '_>,
    >;

    /// 流式版本：文本一到就回调 `on_text`，结束时返回**组装好的完整应答**
    /// （含重新拼起来的 tool 调用和用量）—— 所以 loop 的逻辑两条路径完全一样。
    ///
    /// 默认实现直接退回非流式：没实现流式的 provider 照常可用，
    /// 只是拿不到逐 token 的回调。
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

// ---- provider 目录 ---------------------------------------------------------

/// 两种线上格式。选哪个决定了用哪个 HTTP 客户端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    Anthropic,
    OpenAi,
}

pub struct ProviderInfo {
    pub id: &'static str,
    pub wire: Wire,
    /// 存 key 的环境变量。key 本身绝不进配置文件 —— mcp.json 那种被人贴进
    /// bug 报告的文件里出现 bearer token 就是泄漏。
    ///
    /// **留空 = 本地端点**（Ollama）：它不需要 key，`resolve` 也就不去找。
    pub key_env: &'static str,
    pub base_url: Option<&'static str>,
    /// 主模型（loop 用）
    pub model: &'static str,
    /// 便宜模型（检索门 / consolidation 用）
    pub small_model: &'static str,
    /// 上下文窗口的**近似值**（token）。用途只有一个：算「什么时候该压缩」
    /// （见 `tokens.rs` 与 `JOY_COMPACT_THRESHOLD`）。
    ///
    /// 估小了只会让压缩早发生（多花一次便宜模型调用），估大了才会溢出 ——
    /// 所以取值一律偏保守。真实上限以 provider 自己报的错为准：那类错误会被
    /// 认出来（`ProviderError::ContextOverflow`），压缩后重试一次。
    pub context_window: u32,
}

impl ProviderInfo {
    /// 这个 provider 需不需要 key。本地推理不需要 —— 这正是它能离线、
    /// 零成本、不把对话送出这台机器的原因。
    pub fn needs_key(&self) -> bool {
        !self.key_env.is_empty()
    }
}

/// 这些默认值只是起点，`JOY_MODEL` / `JOY_SMALL_MODEL` 随时覆盖。
/// openrouter 的默认是 `:free` id，一分钱不花也能跑（限速）。
pub static PROVIDERS: &[ProviderInfo] = &[
    ProviderInfo {
        id: "anthropic",
        wire: Wire::Anthropic,
        key_env: "ANTHROPIC_API_KEY",
        base_url: None,
        model: "claude-sonnet-5",
        small_model: "claude-haiku-4-5-20251001",
        context_window: 200_000,
    },
    ProviderInfo {
        id: "openai",
        wire: Wire::OpenAi,
        key_env: "OPENAI_API_KEY",
        base_url: None,
        model: "gpt-5.5",
        small_model: "gpt-4.1-mini",
        context_window: 200_000,
    },
    ProviderInfo {
        id: "openrouter",
        wire: Wire::OpenAi,
        key_env: "OPENROUTER_API_KEY",
        base_url: Some("https://openrouter.ai/api/v1"),
        model: "nvidia/nemotron-3-super-120b-a12b:free",
        small_model: "google/gemma-4-26b-a4b-it:free",
        context_window: 128_000,
    },
    ProviderInfo {
        id: "gemini",
        wire: Wire::OpenAi,
        key_env: "GEMINI_API_KEY",
        base_url: Some("https://generativelanguage.googleapis.com/v1beta/openai/"),
        model: "gemini-3.5-flash",
        small_model: "gemini-3.1-flash-lite",
        context_window: 1_000_000,
    },
    ProviderInfo {
        id: "deepseek",
        wire: Wire::OpenAi,
        key_env: "DEEPSEEK_API_KEY",
        base_url: Some("https://api.deepseek.com"),
        model: "deepseek-v4-pro",
        small_model: "deepseek-v4-pro",
        context_window: 128_000,
    },
    ProviderInfo {
        id: "minimax",
        wire: Wire::Anthropic,
        key_env: "MINIMAX_API_KEY",
        base_url: Some("https://api.minimaxi.com/anthropic"),
        model: "MiniMax-M3",
        small_model: "MiniMax-M2",
        context_window: 200_000,
    },
    ProviderInfo {
        id: "kimi",
        wire: Wire::Anthropic,
        key_env: "MOONSHOT_API_KEY",
        base_url: Some("https://api.moonshot.ai/anthropic"),
        model: "kimi-k3",
        small_model: "kimi-k2.6",
        context_window: 200_000,
    },
    ProviderInfo {
        id: "glm",
        wire: Wire::Anthropic,
        key_env: "ZHIPU_API_KEY",
        base_url: Some("https://api.z.ai/api/anthropic"),
        model: "glm-5.2",
        small_model: "glm-5-turbo",
        context_window: 200_000,
    },
    ProviderInfo {
        id: "xai",
        wire: Wire::OpenAi,
        key_env: "XAI_API_KEY",
        base_url: Some("https://api.x.ai/v1"),
        model: "grok-4",
        small_model: "grok-4-fast",
        context_window: 256_000,
    },
    ProviderInfo {
        id: "opencode_zen",
        wire: Wire::OpenAi,
        key_env: "OPENCODE_ZEN_API_KEY",
        base_url: Some("https://opencode.ai/zen/v1"),
        model: "deepseek-v4-flash-free",
        small_model: "deepseek-v4-flash-free",
        context_window: 128_000,
    },
    ProviderInfo {
        id: "opencode_go",
        wire: Wire::OpenAi,
        key_env: "OPENCODE_GO_API_KEY",
        base_url: Some("https://opencode.ai/zen/go/v1"),
        model: "deepseek-v4-flash",
        small_model: "deepseek-v4-flash",
        context_window: 128_000,
    },
    ProviderInfo {
        // 本地推理：不需要 key，不需要网络。`key_env` 空着就是声明这件事。
        // Ollama 暴露的是 OpenAI 兼容端点（`/v1`），所以复用同一条 wire ——
        // LM Studio / vLLM 也长一样，改 `JOY_BASE_URL` 即可指过去。
        id: "ollama",
        wire: Wire::OpenAi,
        key_env: "",
        base_url: Some("http://127.0.0.1:11434/v1"),
        // 先 `ollama pull qwen3:8b`（主模型）与 `qwen3:4b`（检索门 /
        // consolidation 用的便宜模型）；换成自己装过的名字用 JOY_MODEL。
        model: "qwen3:8b",
        small_model: "qwen3:4b",
        context_window: 32_768,
    },
];

/// key 从哪领。报错时一起给出来，省得用户去搜。
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

#[cfg(test)]
#[path = "provider_tests.rs"]
mod provider_tests;

/// 流式的端到端测试要起本地 HTTP 服务、开真 socket，所以单列一个模块：
/// 它比纯转换测试慢，但覆盖了「解析 → 传输 → 回调」整条链。
#[cfg(test)]
#[path = "streaming_tests.rs"]
mod streaming_tests;

/// 解析完 key / endpoint / 模型之后的东西 —— app-server 拿着它跑 loop。
///
/// `Arc` 而不是 `Box`：图里的节点是 'static 闭包，它得**拥有**一份 client
/// 才活得比这一轮久（triage 的 classify / quick_reply 两条路都要）。
#[derive(Clone)]
pub struct Resolved {
    pub provider_id: String,
    pub client: Arc<dyn Provider>,
    pub model: String,
    pub small_model: String,
}

/// 表里没有的 provider（mock、评测、自建网关）按这个数算上下文窗口。
/// 保守取值：压缩早发生只是多花一次便宜调用，溢出要多跑一整轮请求。
pub const DEFAULT_CONTEXT_WINDOW: u32 = 32_768;

impl Resolved {
    /// 这一轮的上下文窗口近似值。查 `PROVIDERS` 表，查不到就用默认。
    ///
    /// 不把窗口塞进 `Resolved` 的字段里：它本来就住在表里，多一份副本就多一处
    /// 会不一致的地方（而且每加一个 provider 都得记得同步两处）。
    pub fn context_window(&self) -> u32 {
        lookup(&self.provider_id)
            .map(|info| info.context_window)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW)
    }
}

// 手写 Debug：dyn Provider 没法 derive，而测试里 expect_err 需要它。
// client 本身没有值得打印的状态（key 绝不能出现在日志里），所以略去。
impl std::fmt::Debug for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolved")
            .field("provider_id", &self.provider_id)
            .field("model", &self.model)
            .field("small_model", &self.small_model)
            .finish_non_exhaustive()
    }
}

/// 从 Settings 解析出可用的 client。key 缺失时给出**能照着做**的提示：
/// 去哪儿领 key、往哪个文件加哪一行、还有哪些 provider 可选。
pub fn resolve(settings: &Settings) -> Result<Resolved, String> {
    let info = lookup(&settings.provider).ok_or_else(|| {
        format!(
            "未知的 JOY_PROVIDER '{}'。可选：{}",
            settings.provider,
            ids()
        )
    })?;

    // .strip()：复制粘贴带进来的换行/空格会污染请求头。
    //
    // 本地 provider（`key_env` 为空）不需要 key：没有就空着，客户端的
    // `authed` 会因此不发 Authorization 头。显式配了 `JOY_API_KEY` 的
    // 情况仍然照用 —— 有些本地网关（带鉴权的 vLLM 部署）自己要 key。
    let api_key = settings
        .api_key
        .clone()
        .or_else(|| {
            std::env::var(info.key_env)
                .ok()
                .map(|v| v.trim().to_string())
        })
        .filter(|v| !v.is_empty())
        .or_else(|| (!info.needs_key()).then(String::new))
        .ok_or_else(|| no_key_message(info))?;

    let base_url = settings
        .base_url
        .clone()
        .or_else(|| info.base_url.map(str::to_string));
    let timeout = Duration::from_secs(settings.llm_timeout_secs.max(1) as u64);

    let client: Arc<dyn Provider> = match info.wire {
        Wire::Anthropic => Arc::new(anthropic::Client::new(
            &api_key,
            base_url.as_deref(),
            timeout,
        )),
        Wire::OpenAi => Arc::new(openai::Client::new(&api_key, base_url.as_deref(), timeout)),
    };

    Ok(Resolved {
        provider_id: info.id.to_string(),
        client,
        model: settings
            .model
            .clone()
            .unwrap_or_else(|| info.model.to_string()),
        small_model: settings
            .small_model
            .clone()
            .unwrap_or_else(|| info.small_model.to_string()),
    })
}

fn ids() -> String {
    PROVIDERS
        .iter()
        .map(|p| p.id)
        .collect::<Vec<_>>()
        .join(", ")
}

fn no_key_message(info: &ProviderInfo) -> String {
    let mut msg = format!("provider '{}' 没有 API key。\n", info.id);
    if let Some(url) = key_url(info.id) {
        msg.push_str(&format!("  1. 去领一个 key：{url}\n"));
    }
    msg.push_str(&format!(
        "  2. 把它设成环境变量（Joy 不读 .env；要用就先 source）：\n       \
         export {}=你的-key\n",
        info.key_env
    ));
    msg.push_str(&format!("其他 provider：{}", ids()));
    msg.push_str("\n用 JOY_PROVIDER=<name> 切换。");
    msg
}
