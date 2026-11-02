//! SSE 的最小解析：只关心 `data:` 行。
//!
//! Anthropic 和 OpenAI 的流式应答都是同一种外壳：
//!
//! ```text
//! event: content_block_delta
//! data: {"type":"content_block_delta",...}
//! (空行)
//! ```
//!
//! 事件名和结构两家完全不同，但「每行一个 data: 载荷」是一样的，
//! 所以解析到这一层为止，剩下的交给各自的实现。

use bytes::Bytes;
use futures_util::{Stream, StreamExt};

use crate::ProviderError;

/// 把 HTTP 字节流变成一行行 `data:` 载荷。
///
/// `event:` 行、注释、空行都被丢掉 —— 事件边界对调用方没有意义，
/// 它要的只是逐条 JSON。OpenAI 的 `data: [DONE]` 也原样交出去，
/// 由调用方判断。
pub fn data_lines<S>(bytes: S) -> impl Stream<Item = Result<String, ProviderError>>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    futures_util::stream::unfold(
        (bytes, String::new(), false),
        |(mut stream, mut buffer, mut eof)| async move {
            loop {
                // 缓冲里还有整行 → 逐行取。
                if let Some(pos) = buffer.find('\n') {
                    let line: String = buffer.drain(..=pos).collect();
                    let line = line.trim_end_matches(['\n', '\r']);
                    if let Some(payload) = line.strip_prefix("data:") {
                        let payload = payload.trim_start();
                        if payload.is_empty() {
                            continue;
                        }
                        return Some((Ok(payload.to_string()), (stream, buffer, eof)));
                    }
                    continue;
                }

                if eof {
                    // 某些实现在最后一个事件后不带空行 —— 把残留交出去。
                    if let Some(payload) = buffer.strip_prefix("data:") {
                        let payload = payload.trim().to_string();
                        return Some((Ok(payload), (stream, String::new(), eof)));
                    }
                    return None;
                }

                match stream.next().await {
                    Some(Ok(chunk)) => buffer.push_str(&String::from_utf8_lossy(&chunk)),
                    Some(Err(e)) => {
                        return Some((Err(ProviderError::Network(e)), (stream, buffer, eof)))
                    }
                    None => eof = true,
                }
            }
        },
    )
}

#[cfg(test)]
#[path = "sse_tests.rs"]
mod sse_tests;
