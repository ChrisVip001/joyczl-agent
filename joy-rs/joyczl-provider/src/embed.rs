//! 向量化：混合检索的另一条腿。
//!
//! 走 OpenAI 兼容的 `POST {base}/embeddings`（`{"model":…, "input":…}`）——
//! Ollama、OpenAI、以及大多数兼容网关都认这个形状，所以这里不需要为每家
//! 写一个实现。
//!
//! 刻意的克制：**失败就是失败**，没有重试、没有降级。向量是关键词检索的
//! 加分项，算不出来时那条腿短了，检索本身照样工作（见 memory 的检索融合）。

use std::time::Duration;

use reqwest::Client as HttpClient;
use serde_json::json;

use crate::{ProviderError, PROVIDERS};

pub struct Embedder {
    http: HttpClient,
    base_url: String,
    api_key: String,
    model: String,
}

impl Embedder {
    pub fn new(base_url: &str, api_key: &str, model: &str, timeout: Duration) -> Self {
        Self {
            http: HttpClient::builder()
                .timeout(timeout)
                .build()
                .expect("构建 HTTP 客户端"),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
        }
    }

    /// 从设置里装配。`JOY_EMBED_MODEL` 决定模型，端点默认跟当前 provider
    /// 走（本地 Ollama 与云端服务都成立）。
    pub fn from_settings(settings: &joyczl_config::Settings) -> Result<Self, String> {
        let model = settings
            .embed_model
            .clone()
            .ok_or("没有配 embedding 模型：设 JOY_EMBED_MODEL，例如 nomic-embed-text")?;
        let info = crate::lookup(&settings.provider);
        let base = settings
            .base_url
            .clone()
            .or_else(|| info.and_then(|i| i.base_url.map(str::to_string)))
            .ok_or("这个 provider 没有默认端点：用 JOY_BASE_URL 指名 embedding 服务")?;
        // key 没有也照发（本地服务不需要）——跟 chat 那边同一条规矩。
        let key = settings
            .api_key
            .clone()
            .filter(|k| !k.is_empty())
            .or_else(|| {
                info.filter(|i| i.needs_key())
                    .and_then(|i| std::env::var(i.key_env).ok())
                    .map(|k| k.trim().to_string())
            })
            .unwrap_or_default();
        Ok(Self::new(
            &base,
            &key,
            &model,
            Duration::from_secs(settings.llm_timeout_secs.max(1) as u64),
        ))
    }

    pub async fn embed(&self, text: &str) -> Result<Vec<f32>, ProviderError> {
        let mut request = self
            .http
            .post(format!("{}/embeddings", self.base_url))
            .json(&json!({"model": self.model, "input": text}));
        if !self.api_key.is_empty() {
            request = request.bearer_auth(&self.api_key);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let body = response.text().await?;
        if status >= 400 {
            return Err(ProviderError::Http { status, body });
        }
        let parsed: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| ProviderError::Parse(format!("embedding 应答不是 JSON：{e}")))?;
        let vector: Vec<f32> = parsed
            .get("data")
            .and_then(|d| d.get(0))
            .and_then(|first| first.get("embedding"))
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_f64())
                    .map(|v| v as f32)
                    .collect()
            })
            .unwrap_or_default();
        if vector.is_empty() {
            return Err(ProviderError::Api(format!(
                "embedding 应答里没有向量：{body}"
            )));
        }
        Ok(vector)
    }
}

/// 这个 provider 有没有默认端点可以当 embedding 服务用。
pub fn default_endpoint(provider: &str) -> Option<&'static str> {
    PROVIDERS
        .iter()
        .find(|info| info.id == provider)
        .and_then(|info| info.base_url)
}
