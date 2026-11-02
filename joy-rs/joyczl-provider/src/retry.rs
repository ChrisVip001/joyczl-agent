//! 分级退避重试：**有限、可见、不换厂商**。
//!
//! 只有两类错误值得重试（见 `ProviderError::retryable`）：限流（429）与服务端
//! 临时故障（5xx），加上网络抖动。别的错误重试只会以同样方式再失败一次 ——
//! 白等一倍时间，还多花一次配额。
//!
//! 三条自我约束：
//!
//! * **有限**：默认最多 2 次（`JOY_LLM_RETRIES`，上限 5），指数退避 500ms 起、
//!   单次等待上限 8 秒、整轮重试总预算 30 秒。服务端在 `Retry-After` 里给的
//!   秒数会被尊重，但同样受这两个上限约束 —— 一个说「600 秒后再来」的服务端
//!   不该把一轮对话挂在那儿。
//! * **可见**：每次重试都发一条通知（`RetryNotice`）。绝不静默重试 ——
//!   用户看到的应该是「限流了，正在重试」，而不是一个莫名其妙的长时间停顿。
//! * **不换厂商**：换 provider 是另一个决定（要配 fallback 链、要解释为什么
//!   换），这里只做「同一个请求再试一次」。
//!
//! 通知的出口用 **task-local** 而不是 `Provider` trait 的参数：`Provider` 的
//! 签名是与 loop 的契约，不值得为一个通知动它；而「这一轮」正好就是任务边界
//! （`run_turn` 跑在自己的任务里），task-local 天然是「这一轮」的作用域，
//! 也不会像共享字段那样在两个并发 turn 之间串台。

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::ProviderError;

/// 指数退避的起点。
const BASE_DELAY: Duration = Duration::from_millis(500);
/// 单次等待的上限（Retry-After 也受它约束）。
const MAX_DELAY: Duration = Duration::from_secs(8);
/// 整轮重试的时间预算：超过就放弃，让错误如实冒上去。
const TOTAL_BUDGET: Duration = Duration::from_secs(30);

/// 一次重试的通知内容。字段够人看懂发生了什么，也够界面画一行字。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryNotice {
    /// 第几次重试（从 1 开始）。
    pub attempt: u32,
    /// 为什么重试（`HTTP 429` / `网络错误：…`）。
    pub reason: String,
    /// 等了多久。
    pub delay_ms: u64,
}

pub type NoteSink = Arc<dyn Fn(RetryNotice) + Send + Sync>;

tokio::task_local! {
    /// 当前这一轮的通知出口。没有它就退到 stderr（绝不静默）。
    static NOTE_SINK: NoteSink;
}

/// 在这一轮里挂上通知出口。`run_turn` 用它把重试接到 `EventSink` 上。
pub async fn with_note_sink<F: Future>(sink: NoteSink, future: F) -> F::Output {
    NOTE_SINK.scope(sink, future).await
}

/// 发一个请求，按需退避重试。
///
/// `send` 每次都要**重新构造**请求（reqwest 的 `RequestBuilder` 只能消费一次），
/// 所以它是个 `FnMut` 而不是一个现成的 future。返回的是**还没读 body** 的应答。
pub async fn with_retries<F, Fut>(
    max_retries: u32,
    mut send: F,
) -> Result<reqwest::Response, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<reqwest::Response, ProviderError>>,
{
    let started = Instant::now();
    let mut attempt = 0u32;
    loop {
        let error = match send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                if status < 400 {
                    return Ok(response);
                }
                let retry_after = retry_after_secs(&response);
                // body 是给人看的诊断信息：读不出来也不该变成另一类错误。
                let body = response.text().await.unwrap_or_default();
                ProviderError::from_http_retry(status, body, retry_after)
            }
            Err(error) => error,
        };

        match next_delay(&error, attempt, max_retries, started) {
            Some(delay) => {
                note_retry(&error, attempt + 1, delay);
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            None => return Err(error),
        }
    }
}

/// 下一次该等多久；不该重试（或预算用完）返回 `None`。
fn next_delay(
    error: &ProviderError,
    attempt: u32,
    max_retries: u32,
    started: Instant,
) -> Option<Duration> {
    if !error.retryable() || attempt >= max_retries {
        return None;
    }
    // 500ms、1s、2s、4s、8s…封顶 8s。
    let backoff = (BASE_DELAY * 2u32.pow(attempt.min(4))).min(MAX_DELAY);
    let delay = error.retry_after().unwrap_or(backoff).min(MAX_DELAY);
    if started.elapsed() + delay > TOTAL_BUDGET {
        return None;
    }
    Some(delay)
}

/// `Retry-After` 的秒数形式。HTTP-date 形式没解析 —— 有退避兜底。
fn retry_after_secs(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
}

/// 把「正在重试」说出去。这一条是硬要求：重试绝不能静默发生。
fn note_retry(error: &ProviderError, attempt: u32, delay: Duration) {
    let notice = RetryNotice {
        attempt,
        reason: reason_of(error),
        delay_ms: delay.as_millis() as u64,
    };
    if NOTE_SINK.try_with(|sink| sink(notice.clone())).is_err() {
        // 没有这一轮的通知出口（评测、单测、库用法）：退到 stderr。
        eprintln!(
            "(joy) 模型调用失败（{}），{}ms 后重试第 {} 次",
            notice.reason, notice.delay_ms, notice.attempt
        );
    }
}

fn reason_of(error: &ProviderError) -> String {
    match error {
        ProviderError::Http { status, .. } => format!("HTTP {status}"),
        ProviderError::Network(e) => format!("网络错误：{e}"),
        other => other.to_string(),
    }
}
