//! Native, stateless Responses API. Local working memory retains encrypted reasoning items.
//! Sources: https://developers.openai.com/api/docs/guides/function-calling
//! https://developers.openai.com/api/docs/guides/migrate-to-responses
use crate::{
    ContentBlock, CreateRequest, CreateResponse, Provider, ProviderError, Role, StopReason,
    TextSink, Usage,
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::time::Duration;

pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    max_retries: u32,
}
impl Client {
    pub fn new(api_key: &str, base_url: Option<&str>, timeout: Duration, max_retries: u32) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("build HTTP client"),
            base_url: base_url
                .unwrap_or("https://api.openai.com/v1")
                .trim_end_matches('/')
                .to_string(),
            api_key: api_key.into(),
            max_retries,
        }
    }
    async fn send(&self, body: &Value) -> Result<reqwest::Response, ProviderError> {
        let url = format!("{}/responses", self.base_url);
        crate::retry::with_retries(self.max_retries, || async {
            let request = self.http.post(&url).json(body);
            let request = if self.api_key.is_empty() {
                request
            } else {
                request.bearer_auth(&self.api_key)
            };
            Ok(request.send().await?)
        })
        .await
    }
    async fn create_inner(&self, request: CreateRequest) -> Result<CreateResponse, ProviderError> {
        let response = self.send(&to_responses(&request)?).await?;
        from_responses(&response.json::<Value>().await?)
    }
    async fn stream_inner(
        &self,
        request: CreateRequest,
        on_text: TextSink,
    ) -> Result<CreateResponse, ProviderError> {
        let mut body = to_responses(&request)?;
        body["stream"] = json!(true);
        let response = self.send(&body).await?;
        // Never retry after output has started; a dropped stream is an error, not a completed answer.
        let mut events = Box::pin(crate::sse::data_lines(Box::pin(response.bytes_stream())));
        while let Some(event) = events.as_mut().next().await {
            let data = event?;
            if data.trim() == "[DONE]" {
                break;
            }
            let event: Value = serde_json::from_str(&data)
                .map_err(|e| ProviderError::Parse(format!("Responses SSE is not JSON: {e}")))?;
            match event.get("type").and_then(Value::as_str) {
                Some("response.output_text.delta" | "response.refusal.delta") => {
                    if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                        on_text(delta);
                    }
                }
                Some("response.completed" | "response.incomplete") => {
                    let response = event.get("response").ok_or_else(|| {
                        ProviderError::Parse("Responses terminal event has no response".into())
                    })?;
                    return from_responses(response);
                }
                Some("error" | "response.failed") => {
                    return Err(ProviderError::Api(
                        event
                            .get("error")
                            .or_else(|| event.pointer("/response/error"))
                            .unwrap_or(&event)
                            .to_string(),
                    ))
                }
                _ => {}
            }
        }
        Err(ProviderError::Parse(
            "Responses stream ended before a terminal response".into(),
        ))
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

/// Preserve item ordering: reasoning, assistant text, calls, outputs, subsequent messages.
pub fn to_responses(request: &CreateRequest) -> Result<Value, ProviderError> {
    let mut input = Vec::new();
    for message in &request.messages {
        let mut parts = Vec::new();
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let flush = |parts: &mut Vec<Value>, input: &mut Vec<Value>| {
            if !parts.is_empty() {
                let content = if role == "assistant" {
                    let text = parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<String>();
                    parts.clear();
                    Value::String(text)
                } else {
                    Value::Array(std::mem::take(parts))
                };
                input.push(json!({"role": role, "content": content}));
            }
        };
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => parts.push(if message.role == Role::Assistant {
                    json!({"type":"output_text", "text": text, "annotations": []})
                } else { json!({"type":"input_text", "text": text}) }),
                ContentBlock::Image { .. } if message.role == Role::Assistant => return Err(ProviderError::Api("Assistant image output is unsupported by this Responses adapter".into())),
                ContentBlock::Image { media_type, data } => parts.push(json!({"type":"input_image", "image_url":format!("data:{media_type};base64,{data}")})),
                ContentBlock::ToolUse { id, name, input: args, extra } => {
                    flush(&mut parts, &mut input);
                    input.push(extra.as_ref().and_then(|extra| extra.get("openaiResponses")).cloned().unwrap_or_else(|| json!({"type":"function_call", "call_id":id, "name":name, "arguments":args.to_string()})));
                }
                ContentBlock::ToolResult { tool_use_id, content } => {
                    flush(&mut parts, &mut input);
                    input.push(json!({"type":"function_call_output", "call_id":tool_use_id, "output":content}));
                }
                ContentBlock::OpenAiReasoning { .. } => return Err(ProviderError::Api("Chat Completions reasoning cannot be replayed through Responses".into())),
                ContentBlock::ResponseItem { item } => {
                    flush(&mut parts, &mut input);
                    if item.get("type").and_then(Value::as_str) != Some("reasoning") { return Err(ProviderError::Api("Unsupported Responses continuation item".into())); }
                    input.push(item.clone());
                }
            }
        }
        flush(&mut parts, &mut input);
    }
    let tools: Vec<_> = request.tools.iter().map(|tool| json!({"type":"function", "name":tool.name, "description":tool.description, "parameters":tool.input_schema, "strict":false})).collect();
    let instructions = [
        request.cache_prefix.as_deref().unwrap_or(""),
        request.system.as_deref().unwrap_or(""),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join("\n\n");
    Ok(
        json!({"model":request.model, "instructions":instructions, "input":input, "tools":tools,
        "max_output_tokens": request.max_tokens.max(1), "store":false, "include":["reasoning.encrypted_content"]}),
    )
}

pub fn from_responses(value: &Value) -> Result<CreateResponse, ProviderError> {
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        return Err(ProviderError::Api(error.to_string()));
    }
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::Parse("Responses status is missing".into()))?;
    if !matches!(status, "completed" | "incomplete") {
        return Err(ProviderError::Api(format!(
            "Responses did not complete: {status}"
        )));
    }
    let output = value
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::Parse("Responses output is not an array".into()))?;
    let mut content = Vec::new();
    let mut call_ids = std::collections::HashSet::new();
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(text) = part
                        .get("text")
                        .or_else(|| part.get("refusal"))
                        .and_then(Value::as_str)
                    {
                        content.push(ContentBlock::Text { text: text.into() });
                    }
                }
            }
            Some("reasoning") => {
                if item
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .is_none()
                {
                    return Err(ProviderError::Parse("Responses reasoning has no encrypted continuation; endpoint must support reasoning.encrypted_content with store=false".into()));
                }
                content.push(ContentBlock::ResponseItem { item: item.clone() });
            }
            Some("function_call") => {
                if item
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| status != "completed")
                {
                    return Err(ProviderError::Parse(
                        "Responses function call is incomplete; no tool will execute".into(),
                    ));
                }
                let field = |name| {
                    item.get(name)
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .ok_or_else(|| {
                            ProviderError::Parse(format!("Responses function call missing {name}"))
                        })
                };
                let args = field("arguments")?;
                let args = serde_json::from_str::<Value>(&args).map_err(|e| {
                    ProviderError::Parse(format!("Responses tool arguments are not JSON: {e}"))
                })?;
                let call_id = field("call_id")?;
                if call_id.is_empty() || !call_ids.insert(call_id.clone()) {
                    return Err(ProviderError::Parse(
                        "Responses function call identity is empty or duplicated".into(),
                    ));
                }
                content.push(ContentBlock::ToolUse {
                    id: call_id,
                    name: field("name")?,
                    input: args,
                    extra: Some(json!({"openaiResponses":item})),
                });
            }
            kind => {
                return Err(ProviderError::Parse(format!(
                    "Unsupported Responses output item: {kind:?}"
                )))
            }
        }
    }
    let usage = value.get("usage").unwrap_or(&Value::Null);
    let number = |field| usage.get(field).and_then(Value::as_i64).unwrap_or(0);
    let stop_reason = if content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
    {
        StopReason::ToolUse
    } else if status == "incomplete" {
        StopReason::MaxTokens
    } else {
        StopReason::EndTurn
    };
    Ok(CreateResponse {
        content,
        stop_reason,
        usage: Usage {
            input_tokens: number("input_tokens"),
            output_tokens: number("output_tokens"),
            cache_read_input_tokens: usage
                .pointer("/input_tokens_details/cached_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            ..Default::default()
        },
    })
}

#[cfg(test)]
#[path = "responses_tests.rs"]
mod tests;
