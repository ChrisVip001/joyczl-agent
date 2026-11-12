//! Provider errors. Deliberately distinguishes "our side's problem" from "the model side's".

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProviderError {
    /// The model side answered non-2xx. The body is carried verbatim — rate limits, bad
    /// model names, invalid keys: every vendor's hints live in the body; dropping it = guesswork.
    #[error("HTTP {status}: {body}")]
    Http {
        status: u16,
        body: String,
        /// The `Retry-After` header (seconds). Common on 429; if present, wait as the server says.
        retry_after_secs: Option<u64>,
    },

    /// The network layer failed: timeout, DNS, connection refused.
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    /// The provider says "this request exceeds the context window". Its own variant: it is
    /// the **only** error worth "compact and retry once" upstream — retrying others is futile.
    #[error("context exceeded the window (HTTP {status}): {body}")]
    ContextOverflow { status: u16, body: String },

    /// 2xx but wrong content (e.g. OpenRouter rate limits return 200 + error body, no choices).
    #[error("{0}")]
    Api(String),

    /// The response format did not match expectations.
    #[error("could not parse the response: {0}")]
    Parse(String),
}

impl ProviderError {
    /// Build an error from an HTTP response: first check whether it is a "context overflow".
    ///
    /// Every `status >= 400` goes through here, so "detecting overflow" has one implementation.
    pub fn from_http(status: u16, body: String) -> Self {
        Self::from_http_retry(status, body, None)
    }

    /// Same, but carries the seconds the server demanded in `Retry-After` (common on 429).
    pub fn from_http_retry(status: u16, body: String, retry_after_secs: Option<u64>) -> Self {
        if looks_like_context_overflow(status, &body) {
            Self::ContextOverflow { status, body }
        } else {
            Self::Http {
                status,
                body,
                retry_after_secs,
            }
        }
    }

    /// Is this a "context overflow"? (`full_turn` uses it to decide on compact-and-retry)
    pub fn is_context_overflow(&self) -> bool {
        matches!(self, ProviderError::ContextOverflow { .. })
    }

    /// **Worth a backoff retry?** Only two classes: rate limiting (429) and transient server
    /// faults (5xx), plus network-layer jitter.
    ///
    /// Retrying other errors only fails again the same way — doubled wait, one more quota.
    /// Especially never retry "context overflow": that calls for compacting, not slamming again.
    pub fn retryable(&self) -> bool {
        match self {
            ProviderError::Http { status, .. } => *status == 429 || *status >= 500,
            ProviderError::Network(_) => true,
            _ => false,
        }
    }

    /// The wait the server demanded (`Retry-After`); `None` when not given.
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            ProviderError::Http {
                retry_after_secs: Some(secs),
                ..
            } => Some(std::time::Duration::from_secs(*secs)),
            _ => None,
        }
    }
}

/// Detect "context overflow" from an error body.
///
/// Two gates: status code first (only 400 / 413 / 422 genuinely mean "request too large";
/// the same phrase elsewhere is usually coincidence — a false positive wastes a compaction),
/// then the phrase list (vendors word it differently). Missing one costs at most a retry.
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
        // Gateways behind Chinese products word it in Chinese; the English needles above do
        // not cover them, and a missed match only costs a retry.
        "上下文长度",
        "超出上下文",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}
