//! Embeddings: the other leg of hybrid retrieval.
//!
//! Uses the OpenAI-compatible `POST {base}/embeddings` (`{"model":…, "input":…}`) —
//! Ollama, OpenAI, and most compatible gateways accept this shape, so no per-vendor
//! implementation is needed here.
//!
//! Deliberate restraint: **failure is failure** — no retries, no fallbacks. Vectors are a
//! bonus for keyword retrieval; short that leg, retrieval itself still works (memory fusion).

use std::time::Duration;

use reqwest::Client as HttpClient;
use serde_json::json;

use crate::ProviderError;

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

    /// Assemble from settings. `JOY_EMBED_MODEL` picks the model; the endpoint defaults to
    /// the current provider's (true for local Ollama and cloud services alike).
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
            .ok_or("这个 provider 没有默认端点：在 <home>/models.json 里给这套模型配 base_url（或 /model add 时带上）")?;
        // Send even without a key (local services need none) — the same rule as chat.
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
            return Err(ProviderError::from_http(status, body));
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
