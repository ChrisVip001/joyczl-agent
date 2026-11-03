//! Minimal SSE parsing: only `data:` lines matter.
//!
//! Anthropic and OpenAI streaming answers share the same envelope:
//!
//! ```text
//! event: content_block_delta
//! data: {"type":"content_block_delta",...}
//! (blank line)
//! ```
//!
//! Event names and structures differ completely, but "one data: payload per line" is the
//! same — parsing stops at this layer; the rest belongs to each implementation.

use bytes::Bytes;
use futures_util::{Stream, StreamExt};

use crate::ProviderError;

/// Turn the HTTP byte stream into one `data:` payload per item.
///
/// `event:` lines, comments, and blank lines are dropped — event boundaries mean nothing
/// to the caller, which wants JSON items one by one. OpenAI's `data: [DONE]` is passed
/// through verbatim too; the caller decides.
pub fn data_lines<S>(bytes: S) -> impl Stream<Item = Result<String, ProviderError>>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    futures_util::stream::unfold(
        (bytes, String::new(), false),
        |(mut stream, mut buffer, mut eof)| async move {
            loop {
                // A whole line is still in the buffer → take it line by line.
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
                    // Some implementations omit the blank line after the last event — flush the rest.
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
