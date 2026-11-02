//! Anthropic Messages API 的原生实现。
//!
//! 因为 loop 的方言就是 Anthropic 的形状，这里几乎不用转换：
//! `Message` / `ContentBlock` 的 serde 标注（`tag = "type"` + snake_case）
//! 让它们序列化出来就是线上的样子。

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::Client as HttpClient;
use serde_json::{json, Value};

use crate::{
    sse, ContentBlock, CreateRequest, CreateResponse, Provider, ProviderError, StopReason,
    TextSink, Usage,
};

const DEFAULT_BASE: &str = "https://api.anthropic.com";
/// 固定版本号，不追新 —— 升级 API 版本是有行为的变更，该是显式的一次提交。
const API_VERSION: &str = "2023-06-01";

pub struct Client {
    http: HttpClient,
    base_url: String,
    api_key: String,
    /// 限流/临时故障时最多重试几次（`JOY_LLM_RETRIES`）。
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

    /// 发一次请求（限流/临时故障会退避重试），返回**还没读 body** 的应答。
    async fn send(&self, body: &Value) -> Result<reqwest::Response, ProviderError> {
        let url = format!("{}/v1/messages", self.base_url);
        crate::retry::with_retries(self.max_retries, || async {
            Ok(self
                .http
                .post(&url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", API_VERSION)
                .json(body)
                .send()
                .await?)
        })
        .await
    }

    /// 一次性调用用的包装。
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
        let mut body = json!({
            "model": request.model,
            "max_tokens": request.max_tokens,
            "messages": request.messages,
        });
        if let Some(system) = &request.system {
            body["system"] = json!(system);
        }
        // 空的 tools 数组有些端点会拒绝，所以干脆不发这个键。
        if !request.tools.is_empty() {
            body["tools"] = json!(request.tools);
        }

        let text = self.post(&body).await?;

        let value: Value = serde_json::from_str(&text)
            .map_err(|e| ProviderError::Parse(format!("应答不是合法 JSON：{e}")))?;
        parse_message(&value)
    }

    /// SSE 流式。文本一到就回调，工具调用的参数是分片来的
    /// （`input_json_delta` 的 `partial_json`），按 index 攒齐后再一起解析。
    async fn stream_inner(
        &self,
        request: CreateRequest,
        on_text: TextSink,
    ) -> Result<CreateResponse, ProviderError> {
        let mut body = json!({
            "model": request.model,
            "max_tokens": request.max_tokens,
            "messages": request.messages,
            "stream": true,
        });
        if let Some(system) = &request.system {
            body["system"] = json!(system);
        }
        if !request.tools.is_empty() {
            body["tools"] = json!(request.tools);
        }

        // 重试只覆盖「拿到应答」这一步；开始读流之后断开就不再重发。
        let response = self.send(&body).await?;

        // index → (id, name, 到目前为止攒到的参数 JSON)
        let mut tools: BTreeMap<i64, (String, String, String)> = BTreeMap::new();
        let mut text = String::new();
        let mut usage = Usage::default();
        let mut stop_reason = StopReason::EndTurn;

        let mut events = Box::pin(sse::data_lines(Box::pin(response.bytes_stream())));
        while let Some(item) = events.as_mut().next().await {
            let data = item?;
            let value: Value = serde_json::from_str(&data)
                .map_err(|e| ProviderError::Parse(format!("SSE 载荷不是 JSON：{e}")))?;

            match value.get("type").and_then(|t| t.as_str()) {
                Some("message_start") => {
                    usage.input_tokens = value
                        .pointer("/message/usage/input_tokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                }
                Some("content_block_start") => {
                    let index = value.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                    let block = value.get("content_block").cloned().unwrap_or(Value::Null);
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        let id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        tools.insert(index, (id, name, String::new()));
                    }
                }
                Some("content_block_delta") => {
                    let index = value.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                    let delta = value.get("delta").cloned().unwrap_or(Value::Null);
                    match delta.get("type").and_then(|t| t.as_str()) {
                        Some("text_delta") => {
                            let piece = delta
                                .get("text")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default();
                            if !piece.is_empty() {
                                text.push_str(piece);
                                on_text(piece);
                            }
                        }
                        Some("input_json_delta") => {
                            if let Some(entry) = tools.get_mut(&index) {
                                entry.2.push_str(
                                    delta
                                        .get("partial_json")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default(),
                                );
                            }
                        }
                        // thinking_delta 等先忽略 —— 思考块不该进上下文。
                        _ => {}
                    }
                }
                Some("message_delta") => {
                    if let Some(reason) =
                        value.pointer("/delta/stop_reason").and_then(|v| v.as_str())
                    {
                        stop_reason = super::anthropic::stop_reason(reason);
                    }
                    if let Some(out) = value
                        .pointer("/usage/output_tokens")
                        .and_then(|v| v.as_i64())
                    {
                        usage.output_tokens = out;
                    }
                }
                Some("error") => return Err(ProviderError::Api(value.to_string())),
                // ping / message_stop / content_block_stop 都不用管
                _ => {}
            }
        }

        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(ContentBlock::Text { text });
        }
        for (_, (id, name, arguments)) in tools {
            let input = serde_json::from_str(&arguments).unwrap_or_else(|_| json!({}));
            content.push(ContentBlock::ToolUse {
                id,
                name,
                input,
                extra: None,
            });
        }

        Ok(CreateResponse {
            stop_reason,
            usage,
            content,
        })
    }
}

/// 从 Anthropic 的应答体里取出 loop 需要的三样：内容块、停因、用量。
/// 拆成独立函数以便单测 —— 不用为它起一个真的 HTTP 服务。
pub fn parse_message(value: &Value) -> Result<CreateResponse, ProviderError> {
    let content: Vec<ContentBlock> =
        serde_json::from_value(value.get("content").cloned().unwrap_or(Value::Null))
            .map_err(|e| ProviderError::Parse(format!("content 块不对：{e}")))?;

    let stop_reason = value
        .get("stop_reason")
        .and_then(|s| s.as_str())
        .map(stop_reason)
        .unwrap_or(StopReason::EndTurn);

    let usage = value
        .get("usage")
        .cloned()
        .and_then(|u| serde_json::from_value::<Usage>(u).ok())
        .unwrap_or_default();

    Ok(CreateResponse {
        stop_reason,
        usage,
        content,
    })
}

pub fn stop_reason(wire: &str) -> StopReason {
    match wire {
        "end_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        _ => StopReason::Other,
    }
}
