//! A scripted fake provider.
//!
//! Tests for loop and the retrieval gate rely on it: no HTTP server, no real key, yet
//! precise control over the model's every turn — free fallout from one trait for model calls.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Mutex;

use serde_json::Value;

use crate::{
    ContentBlock, CreateRequest, CreateResponse, Provider, ProviderError, StopReason, TextSink,
    Usage,
};

/// One item in the queue: either a response or a **failure** — paths like "context
/// overflow → compact → retry" must be scriptable.
pub type MockOutcome = Result<CreateResponse, ProviderError>;

pub struct Mock {
    /// Queued responses pop in order; empty means an error — "the model said more than the
    /// test expected" then surfaces immediately instead of silently reusing the last entry.
    responses: Mutex<VecDeque<MockOutcome>>,
    /// Received requests, for assertions (e.g. confirming system and tools really arrived).
    pub received: Mutex<Vec<CreateRequest>>,
    /// true = `stream()` calls back with text split into small chunks, to test streaming.
    deltas: bool,
    /// A mark every time stream() is called — tests assert "streaming really happened" with it.
    pub streamed: Mutex<Vec<bool>>,
}

impl Mock {
    pub fn new(responses: Vec<CreateResponse>) -> Self {
        Self::with_outcomes(responses.into_iter().map(Ok).collect())
    }

    /// Hand-build a queue of "success or failure".
    pub fn with_outcomes(outcomes: Vec<MockOutcome>) -> Self {
        Self {
            responses: Mutex::new(outcomes.into()),
            received: Mutex::new(Vec::new()),
            deltas: false,
            streamed: Mutex::new(Vec::new()),
        }
    }

    /// Same as `new`, but takes the streaming path (chunked text callbacks).
    pub fn streaming(responses: Vec<CreateResponse>) -> Self {
        Self {
            deltas: true,
            ..Self::new(responses)
        }
    }

    /// A "context exceeds window" failure — real providers do report it this way.
    pub fn context_overflow() -> MockOutcome {
        Err(ProviderError::ContextOverflow {
            status: 400,
            body: "This model's maximum context length is 200000 tokens".to_string(),
        })
    }

    fn take(&self, request: CreateRequest) -> Result<CreateResponse, ProviderError> {
        self.received.lock().expect("锁").push(request);
        self.responses
            .lock()
            .expect("锁")
            .pop_front()
            .unwrap_or_else(|| Err(ProviderError::Api("mock 的应答用完了".to_string())))
    }

    /// A plain-text response, the most common shape.
    pub fn text(text: &str) -> CreateResponse {
        CreateResponse {
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
            },
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    /// One tool call.
    pub fn tool_use(id: &str, name: &str, input: Value) -> CreateResponse {
        CreateResponse {
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 20,
                output_tokens: 8,
            },
            content: vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input,
                extra: None,
            }],
        }
    }
}

impl Provider for Mock {
    fn create(
        &self,
        request: CreateRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<CreateResponse, ProviderError>> + Send + '_>>
    {
        let response = self.take(request);
        Box::pin(std::future::ready(response))
    }

    fn stream(
        &self,
        request: CreateRequest,
        on_text: TextSink,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<CreateResponse, ProviderError>> + Send + '_>>
    {
        let response = self.take(request);
        self.streamed.lock().expect("锁").push(true);
        let text = match response.as_ref().ok().and_then(|r| r.content.first()) {
            Some(ContentBlock::Text { text }) => Some(text.clone()),
            _ => None,
        };
        if self.deltas {
            if let Some(text) = text {
                // Two chars per chunk, small sleeps between: await points like real SSE and
                // scheduling headroom for the consumer, so turn/interrupt cancellation tokens
                // can deterministically cut into the stream mid-flight.
                return Box::pin(async move {
                    for chunk in split_chunks(&text, 2) {
                        on_text(&chunk);
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                    response
                });
            }
        }
        Box::pin(std::future::ready(response))
    }
}

fn split_chunks(text: &str, size: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(size.max(1))
        .map(|c| c.iter().collect())
        .collect()
}
