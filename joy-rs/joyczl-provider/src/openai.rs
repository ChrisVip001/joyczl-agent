//! OpenAI 兼容端点的适配器。
//!
//! loop 说 Anthropic 的方言，这里是两种 wire format 之间的全部翻译 ——
//! 两个方向的转换都是纯函数，编译器保证两边字段对得上。
//!
//! Gemini 思考模型会在 tool_call 上带 `thought_signature`，下一轮必须原样
//! 回传否则 400。它进 `ContentBlock::ToolUse` 的 `extra` 字段，这里在
//! `extra_content` 与 `extra` 之间来回搬 —— 别的 wire 看不到它。

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
}

impl Client {
    pub fn new(api_key: &str, base_url: Option<&str>, timeout: Duration) -> Self {
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
        }
    }

    /// 给请求带上 key —— 除非本来就没有。本地端点（Ollama / LM Studio /
    /// vLLM）不需要 key：一个空的 `Authorization: Bearer ` 头比不带头更糟，
    /// 有些网关会因此回 401，而不是「忽略」。
    fn authed(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.api_key.is_empty() {
            builder
        } else {
            builder.bearer_auth(&self.api_key)
        }
    }

    async fn post(&self, body: &Value) -> Result<(u16, String), ProviderError> {
        let response = self
            .authed(
                self.http
                    .post(format!("{}/chat/completions", self.base_url)),
            )
            .json(body)
            .send()
            .await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        Ok((status, text))
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
        let (status, text) = self.post(&body).await?;
        if status >= 400 {
            // 旧端点只认 max_tokens，不认 max_completion_tokens。只在错误
            // 真的是关于这个参数时才重试 —— 见过别的失败被这次重试盖住，
            // 留下一条「用 max_completion_tokens」的迷惑信息。
            let about_tokens =
                text.contains("max_completion_tokens") || text.contains("max_tokens");
            if body.get("max_completion_tokens").is_some() && about_tokens {
                let mut retry = body.clone();
                retry["max_tokens"] = retry["max_completion_tokens"].take();
                let (s2, t2) = self.post(&retry).await?;
                if s2 >= 400 {
                    return Err(ProviderError::Http {
                        status: s2,
                        body: t2,
                    });
                }
                return from_openai(&t2);
            }
            return Err(ProviderError::Http { status, body: text });
        }
        from_openai(&text)
    }

    /// SSE 流式。tool 调用的参数按 index 分片到达，攒齐后再解析。
    /// `include_usage` 让最后一个 chunk 带上用量 —— 不加的话流式拿不到
    /// token 数，trace 上的花费就是 0。
    async fn stream_inner(
        &self,
        request: CreateRequest,
        on_text: TextSink,
    ) -> Result<CreateResponse, ProviderError> {
        let mut body = to_openai(&request);
        body["stream"] = json!(true);
        body["stream_options"] = json!({ "include_usage": true });

        let response = self
            .authed(
                self.http
                    .post(format!("{}/chat/completions", self.base_url)),
            )
            .json(&body)
            .send()
            .await?;
        let status = response.status().as_u16();
        if status >= 400 {
            let text = response.text().await?;
            return Err(ProviderError::Http { status, body: text });
        }

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

            // 用量在最后一个 chunk 上，而那个 chunk 的 choices 是空的。
            if let Some(u) = value.get("usage") {
                if !u.is_null() {
                    usage.input_tokens =
                        u.get("prompt_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                    usage.output_tokens = u
                        .get("completion_tokens")
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

/// 一条 tool 调用流式攒齐前的样子。参数是分片到达的，
/// `arguments` 要一路追加到最后才能解析。
#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
    extra: Option<Value>,
}

/// Anthropic 形状 → OpenAI 形状。
pub fn to_openai(request: &CreateRequest) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(system) = &request.system {
        messages.push(json!({ "role": "system", "content": system }));
    }

    for message in &request.messages {
        match message.role {
            Role::Assistant => {
                // 文本 + tool 调用合成一条 assistant 消息。
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
                            // Gemini 思考模型的 thought_signature —— 上一轮带回来
                            // 的必须原样送回去，否则下一轮 400。
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
                // OpenAI 要求每个 tool 结果单独一条 tool 消息。带 tool 结果的
                // 消息里的纯文本（模型其实看不见）一并丢弃。
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
                    if !text.is_empty() {
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

/// OpenAI 形状 → Anthropic 形状。
pub fn from_openai(text: &str) -> Result<CreateResponse, ProviderError> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| ProviderError::Parse(format!("应答不是合法 JSON：{e}")))?;

    // OpenRouter 限流时回 200 + error body、没有 choices：把它的消息端出来，
    // 别在下面取 choices[0] 时崩掉。
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
