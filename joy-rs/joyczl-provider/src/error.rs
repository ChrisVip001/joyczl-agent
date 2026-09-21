//! provider 的错误。刻意区分「我这边的问题」和「模型那边的问题」。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProviderError {
    /// 模型那边回了非 2xx。body 原样带上 —— 限流、模型名写错、key 无效，
    /// 各家的提示都在 body 里，丢掉它排查就只能靠猜。
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },

    /// 网络层挂了：超时、DNS、连接被拒。
    #[error("网络错误: {0}")]
    Network(#[from] reqwest::Error),

    /// provider 说「这次请求超出上下文窗口」。单独成一类，因为它是**唯一**
    /// 值得上层「压缩后重试一次」的错误 —— 别的错误重试只会得到同样的错误。
    #[error("上下文超出窗口（HTTP {status}）：{body}")]
    ContextOverflow { status: u16, body: String },

    /// 2xx 但内容不对（比如 OpenRouter 限流时回 200 + error body、没有 choices）。
    #[error("{0}")]
    Api(String),

    /// 应答格式与预期不符。
    #[error("应答解析失败: {0}")]
    Parse(String),
}

impl ProviderError {
    /// 从一次 HTTP 应答造错误：先认一认是不是「上下文溢出」。
    ///
    /// 所有 `status >= 400` 的地方都走这里，于是「认出溢出」只有一处实现。
    pub fn from_http(status: u16, body: String) -> Self {
        if looks_like_context_overflow(status, &body) {
            Self::ContextOverflow { status, body }
        } else {
            Self::Http { status, body }
        }
    }

    /// 是「上下文溢出」吗？（`full_turn` 靠它决定要不要压缩后重试）
    pub fn is_context_overflow(&self) -> bool {
        matches!(self, ProviderError::ContextOverflow { .. })
    }
}

/// 从错误体里认出「上下文溢出」。
///
/// 两道门槛：先看状态码（只有 400 / 413 / 422 是真的在说「这次请求太大」，
/// 别处的同一个词多半是巧合 —— 误判会白花一次压缩），再看短语表（各家措辞
/// 不同）。认不出来最多是少一次重试，错误照常冒给用户，不会有别的后果。
pub fn looks_like_context_overflow(status: u16, body: &str) -> bool {
    if !matches!(status, 400 | 413 | 422) {
        return false;
    }
    let lower = body.to_lowercase();
    [
        "context_length_exceeded",
        "maximum context length",
        "context length",
        "prompt is too long",
        "too many tokens",
        "exceeds the maximum",
        "input length",
        "context window",
        "上下文长度",
        "超出上下文",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}
