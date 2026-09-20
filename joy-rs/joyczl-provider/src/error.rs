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

    /// 2xx 但内容不对（比如 OpenRouter 限流时回 200 + error body、没有 choices）。
    #[error("{0}")]
    Api(String),

    /// 应答格式与预期不符。
    #[error("应答解析失败: {0}")]
    Parse(String),
}
