//! Adapter for OpenAI-compatible endpoints.
//!
//! loop speaks Anthropic's dialect; this file is the entire translation between the two
//! wire formats — both directions are pure functions, and the compiler keeps fields aligned.
//!
//! Gemini thinking models attach `thought_signature` to tool_call; the next turn must pass
//! it back verbatim or 400. It goes into `ContentBlock::ToolUse`'s `extra` field, shuttled
//! here between `extra_content` and `extra` — other wires never see it.

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::Client as HttpClient;
use serde_json::{json, Value};

use crate::{
    sse, ContentBlock, CreateRequest, CreateResponse, Provider, ProviderError, Role, StopReason,
    TextSink, Usage,
};

const DEFAULT_BASE: &str = "https://api.openai.com/v1";

pub struct Client {
    http: HttpClient,
    base_url: String,
    api_key: String,
    /// Max retries on rate limits / transient faults (`JOY_LLM_RETRIES`).
    max_retries: u32,
}

impl Client {
    pub fn new(api_key: &str, base_url: Option<&str>, timeout: Duration, max_retries: u32) -> Self {
        Self {
            http: HttpClient::builder()
                .timeout(timeout)
                .build()
                .expect("构建 HTTP 客户端"),
            base_url: base_url
                .unwrap_or(DEFAULT_BASE)
                .trim_end_matches('/')
                .to_string(),
            api_key: api_key.to_string(),
            max_retries,
        }
    }

    /// Attach the key to a request — unless there is none. Local endpoints (Ollama /
    /// LM Studio / vLLM) need no key: an empty `Authorization: Bearer ` header is worse
    /// than none — some gateways answer 401 instead of "ignoring" it.
    fn authed(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.api_key.is_empty() {
            builder
        } else {
            builder.bearer_auth(&self.api_key)
        }
    }

    /// Send one request (retrying on rate limits / transient faults); returns the response
    /// **with its body unread** — the streaming path reads it on its own.
    async fn send(&self, body: &Value) -> Result<reqwest::Response, ProviderError> {
        let url = format!("{}/chat/completions", self.base_url);
        crate::retry::with_retries(self.max_retries, || async {
            Ok(self.authed(self.http.post(&url)).json(body).send().await?)
        })
        .await
    }

    /// Wrapper for one-shot calls: reads the body as text (non-2xx already became an error in `send`).
    async fn post(&self, body: &Value) -> Result<String, ProviderError> {
        let response = self.send(body).await?;
        response.text().await.map_err(ProviderError::from)
    }
}

impl Provider for Client {
    fn create(
        &self,
        request: CreateRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<CreateResponse, ProviderError>> + Send + '_>,
    > {
        Box::pin(self.create_inner(request))
    }

    fn stream(
        &self,
        request: CreateRequest,
        on_text: TextSink,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<CreateResponse, ProviderError>> + Send + '_>,
    > {
        Box::pin(self.stream_inner(request, on_text))
    }
}

impl Client {
    async fn create_inner(&self, request: CreateRequest) -> Result<CreateResponse, ProviderError> {
        let body = to_openai(&request);
        let text = match self.post(&body).await {
            Ok(text) => text,
            // Older endpoints only know max_tokens, not max_completion_tokens. Retry under the
            // other name only when the error is truly about this parameter — seen other failures
            // masked by this retry, leaving a confusing "use max_completion_tokens" message.
            Err(ProviderError::Http {
                status: 400,
                body: why,
                ..
            }) if body.get("max_completion_tokens").is_some()
                && (why.contains("max_completion_tokens") || why.contains("max_tokens")) =>
            {
                let mut retry = body.clone();
                retry["max_tokens"] = retry["max_completion_tokens"].take();
                self.post(&retry).await?
            }
            Err(e) => return Err(e),
        };
        from_openai(&text)
    }

    /// SSE streaming. Tool-call arguments arrive in fragments by index, parsed once complete.
    /// `include_usage` makes the last chunk carry usage — without it streaming has no token
    /// counts, and the cost on a trace would read 0.
    async fn stream_inner(
        &self,
        request: CreateRequest,
        on_text: TextSink,
    ) -> Result<CreateResponse, ProviderError> {
        let mut body = to_openai(&request);
        body["stream"] = json!(true);
        body["stream_options"] = json!({ "include_usage": true });

        // Retries only cover "getting the response": once the stream starts, a drop is not
        // resent — resending would duplicate the half answer already shown to the user.
        let response = self.send(&body).await?;

        let mut tools: BTreeMap<i64, PartialCall> = BTreeMap::new();
        let mut text = String::new();
        let mut usage = Usage::default();
        let mut finish_reason: Option<String> = None;

        let mut events = Box::pin(sse::data_lines(Box::pin(response.bytes_stream())));
        while let Some(item) = events.as_mut().next().await {
            let data = item?;
            if data.trim() == "[DONE]" {
                break;
            }
            let value: Value = serde_json::from_str(&data)
                .map_err(|e| ProviderError::Parse(format!("SSE 载荷不是 JSON：{e}")))?;

            // Usage rides on the last chunk, whose choices array is empty.
            if let Some(u) = value.get("usage") {
                if !u.is_null() {
                    usage.input_tokens =
                        u.get("prompt_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                    usage.output_tokens = u
                        .get("completion_tokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    usage.cache_read_input_tokens = u
                        .pointer("/prompt_tokens_details/cached_tokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                }
            }

            let Some(choice) = value
                .get("choices")
                .and_then(|c| c.as_array())
                .and_then(|a| a.first())
            else {
                continue;
            };
            if let Some(reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                finish_reason = Some(reason.to_string());
            }

            let delta = choice.get("delta").cloned().unwrap_or(Value::Null);
            if let Some(piece) = delta.get("content").and_then(|v| v.as_str()) {
                if !piece.is_empty() {
                    text.push_str(piece);
                    on_text(piece);
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for call in calls {
                    let index = call.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                    let entry = tools.entry(index).or_default();
                    if let Some(id) = call.get("id").and_then(|v| v.as_str()) {
                        entry.id = id.to_string();
                    }
                    if let Some(name) = call.pointer("/function/name").and_then(|v| v.as_str()) {
                        entry.name = name.to_string();
                    }
                    if let Some(args) = call.pointer("/function/arguments").and_then(|v| v.as_str())
                    {
                        entry.arguments.push_str(args);
                    }
                    if let Some(extra) = call.get("extra_content") {
                        entry.extra = Some(extra.clone());
                    }
                }
            }
        }

        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(ContentBlock::Text { text });
        }
        for (_, call) in std::mem::take(&mut tools) {
            let input = serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({}));
            content.push(ContentBlock::ToolUse {
                id: call.id,
                name: call.name,
                input,
                extra: call.extra,
            });
        }

        let stop_reason = match finish_reason.as_deref() {
            Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
            Some("length") | Some("max_tokens") => StopReason::MaxTokens,
            Some("stop") => StopReason::EndTurn,
            _ if !tools.is_empty() => StopReason::ToolUse,
            _ => StopReason::EndTurn,
        };

        Ok(CreateResponse {
            stop_reason,
            usage,
            content,
        })
    }
}

/// A tool call as accumulated during streaming, before completion. Arguments arrive in
/// fragments; `arguments` must be appended all the way to the end before parsing.
#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
    extra: Option<Value>,
}

/// Anthropic shape → OpenAI shape.
pub fn to_openai(request: &CreateRequest) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    // The stable prefix goes first and never moves: that is what makes the provider's
    // automatic prefix cache hit. No `cache_control` here — this wire has no such field,
    // and inventing one is a 400 on some gateways.
    if let Some(system) = request.system_text() {
        messages.push(json!({ "role": "system", "content": system }));
    }

    for message in &request.messages {
        match message.role {
            Role::Assistant => {
                // Text + tool calls merged into a single assistant message.
                let text = message.text();
                let calls: Vec<Value> = message
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse {
                            id,
                            name,
                            input,
                            extra,
                        } => {
                            let mut call = json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": serde_json::to_string(input).unwrap_or_default(),
                                },
                            });
                            // Gemini thinking models' thought_signature — what came back from
                            // the last turn must be sent back verbatim, or the next turn 400s.
                            if let Some(extra) = extra {
                                call["extra_content"] = extra.clone();
                            }
                            Some(call)
                        }
                        _ => None,
                    })
                    .collect();
                let mut entry = json!({ "role": "assistant" });
                entry["content"] = if text.is_empty() {
                    Value::Null
                } else {
                    json!(text)
                };
                if !calls.is_empty() {
                    entry["tool_calls"] = json!(calls);
                }
                messages.push(entry);
            }
            Role::User => {
                // OpenAI requires each tool result as its own tool message. Plain text in a
                // message carrying tool results (invisible to the model) is dropped with it.
                let has_tool_results = message
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult { .. }));
                if has_tool_results {
                    for block in &message.content {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                        } = block
                        {
                            messages.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": content,
                            }));
                        }
                    }
                } else {
                    let text = message.text();
                    let images: Vec<&ContentBlock> = message
                        .content
                        .iter()
                        .filter(|b| matches!(b, ContentBlock::Image { .. }))
                        .collect();
                    if !images.is_empty() {
                        // With images, OpenAI wants **array content**: text parts plus
                        // data-URI image parts. (A message carrying tool results keeps its
                        // per-result tool messages; images only ride on plain user messages,
                        // which is where the loop puts them.)
                        let mut parts: Vec<Value> = Vec::new();
                        if !text.is_empty() {
                            parts.push(json!({ "type": "text", "text": text }));
                        }
                        for image in images {
                            let ContentBlock::Image { media_type, data } = image else {
                                continue;
                            };
                            parts.push(json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{media_type};base64,{data}"),
                                },
                            }));
                        }
                        messages.push(json!({ "role": "user", "content": parts }));
                    } else if !text.is_empty() {
                        messages.push(json!({ "role": "user", "content": text }));
                    }
                }
            }
        }
    }

    let mut body = json!({
        "model": request.model,
        "max_completion_tokens": request.max_tokens,
        "messages": messages,
    });
    if !request.tools.is_empty() {
        body["tools"] = json!(request
            .tools
            .iter()
            .map(|t| json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                },
            }))
            .collect::<Vec<_>>());
    }
    body
}

/// OpenAI shape → Anthropic shape.
pub fn from_openai(text: &str) -> Result<CreateResponse, ProviderError> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| ProviderError::Parse(format!("应答不是合法 JSON：{e}")))?;

    // OpenRouter answers rate limits with 200 + an error body and no choices: surface its
    // message instead of crashing below when taking choices[0].
    if let Some(error) = value.get("error") {
        return Err(ProviderError::Api(error.to_string()));
    }
    let choice = value
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| ProviderError::Api("应答里没有 choices".to_string()))?;
    let message = choice
        .get("message")
        .ok_or_else(|| ProviderError::Api("choice 里没有 message".to_string()))?;

    let mut content = Vec::new();
    if let Some(text) = message.get("content").and_then(|c| c.as_str()) {
        if !text.is_empty() {
            content.push(ContentBlock::Text {
                text: text.to_string(),
            });
        }
    }

    let mut tool_calls = 0;
    if let Some(calls) = message.get("tool_calls").and_then(|c| c.as_array()) {
        for call in calls {
            let id = call
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let name = call
                .pointer("/function/name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let raw = call
                .pointer("/function/arguments")
                .and_then(|v| v.as_str())
                .unwrap_or("{}");
            let input = serde_json::from_str(raw).unwrap_or_else(|_| json!({}));
            let extra = call.get("extra_content").cloned();
            content.push(ContentBlock::ToolUse {
                id,
                name,
                input,
                extra,
            });
            tool_calls += 1;
        }
    }

    let usage = Usage {
        input_tokens: value
            .pointer("/usage/prompt_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        output_tokens: value
            .pointer("/usage/completion_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        cache_read_input_tokens: value
            .pointer("/usage/prompt_tokens_details/cached_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        cache_creation_input_tokens: 0,
    };

    Ok(CreateResponse {
        stop_reason: if tool_calls > 0 {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        },
        usage,
        content,
    })
}
