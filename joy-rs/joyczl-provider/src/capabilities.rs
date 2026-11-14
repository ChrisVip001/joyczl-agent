//! Explicit main-model declarations and admission before paid requests.
use crate::{CreateRequest, CreateResponse, Provider, ProviderError, TextSink, Wire};
use joyczl_protocol::ModelCapabilities;
use std::{future::Future, pin::Pin, sync::Arc};

type Reply<'a> = Pin<Box<dyn Future<Output = Result<CreateResponse, ProviderError>> + Send + 'a>>;

pub fn validate(caps: Option<&ModelCapabilities>, wire: Wire, model: &str) -> Result<(), String> {
    let Some(caps) = caps else {
        return Ok(());
    };
    if caps.context_window.is_some_and(|value| value <= 0) {
        return Err("Model contextWindow must be positive".into());
    }
    if let Some(effort) = caps.reasoning_effort.as_deref() {
        if wire != Wire::Responses {
            return Err("Explicit reasoningEffort currently requires the Responses adapter".into());
        }
        if !["none", "minimal", "low", "medium", "high", "xhigh", "max"].contains(&effort) {
            return Err(
                "Unknown reasoningEffort; use none, minimal, low, medium, high, xhigh or max"
                    .into(),
            );
        }
        if (model.starts_with("gpt-6-astra") && effort == "none")
            || (model.starts_with("gpt-6.1-sol") && ["none", "minimal"].contains(&effort))
        {
            return Err(format!(
                "Model {model} does not support reasoningEffort {effort}"
            ));
        }
    }
    Ok(())
}

pub(crate) struct Checked {
    pub inner: Arc<dyn Provider>,
    pub main_model: String,
    pub tools: bool,
    pub wire: Wire,
}
impl Checked {
    fn admit(&self, request: &CreateRequest) -> Result<(), ProviderError> {
        if !request.tools.is_empty() {
            if request.model == self.main_model && !self.tools {
                return Err(ProviderError::Api("This model profile declares tools=false; select a tool-capable profile for agent tasks".into()));
            }
            if self.wire == Wire::OpenAi
                && (request.model.starts_with("gpt-6-astra")
                    || request.model.starts_with("gpt-6.1-sol"))
            {
                return Err(ProviderError::Api(
                    "This model requires the Responses adapter for function tools".into(),
                ));
            }
        }
        Ok(())
    }
}
impl Provider for Checked {
    fn create(&self, request: CreateRequest) -> Reply<'_> {
        Box::pin(async move {
            self.admit(&request)?;
            self.inner.create(request).await
        })
    }
    fn stream(&self, request: CreateRequest, sink: TextSink) -> Reply<'_> {
        Box::pin(async move {
            self.admit(&request)?;
            self.inner.stream(request, sink).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Counted(AtomicUsize);
    impl Provider for Counted {
        fn create(&self, _request: CreateRequest) -> Reply<'_> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(crate::mock::Mock::text("ok")) })
        }
        fn stream(&self, request: CreateRequest, _sink: TextSink) -> Reply<'_> {
            self.create(request)
        }
    }
    #[tokio::test]
    async fn unsupported_tools_never_reach_the_paid_provider() {
        let inner = Arc::new(Counted(AtomicUsize::new(0)));
        let checked = Checked {
            inner: inner.clone(),
            main_model: "main".into(),
            tools: false,
            wire: Wire::Responses,
        };
        let mut request = CreateRequest {
            model: "main".into(),
            cache_prefix: None,
            system: None,
            messages: vec![crate::Message::user_text("work")],
            tools: vec![crate::ToolSchema {
                name: "write".into(),
                description: "write".into(),
                input_schema: serde_json::json!({"type":"object"}),
            }],
            max_tokens: 10,
        };
        assert!(checked.create(request.clone()).await.is_err());
        assert!(checked
            .stream(request.clone(), Arc::new(|_| {}))
            .await
            .is_err());
        assert_eq!(inner.0.load(Ordering::SeqCst), 0);
        let incompatible = Checked {
            inner: inner.clone(),
            main_model: "gpt-6.1-sol".into(),
            tools: true,
            wire: Wire::OpenAi,
        };
        request.model = "gpt-6.1-sol".into();
        assert!(incompatible.create(request.clone()).await.is_err());
        assert_eq!(inner.0.load(Ordering::SeqCst), 0);
        request.model = "main".into();
        request.tools.clear();
        checked.create(request).await.unwrap();
        assert_eq!(inner.0.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn declarations_reject_invalid_efforts_windows_and_wire_combinations() {
        let mut caps = ModelCapabilities {
            reasoning_effort: Some("high".into()),
            ..Default::default()
        };
        assert!(validate(Some(&caps), Wire::Anthropic, "any").is_err());
        assert!(validate(Some(&caps), Wire::Responses, "gpt-6.1-sol").is_ok());
        caps.reasoning_effort = Some("none".into());
        assert!(validate(Some(&caps), Wire::Responses, "gpt-6-astra").is_err());
        assert!(validate(Some(&caps), Wire::Responses, "gpt-6.1-sol").is_err());
        caps.reasoning_effort = Some("invented".into());
        assert!(validate(Some(&caps), Wire::Responses, "any").is_err());
        caps.reasoning_effort = None;
        caps.context_window = Some(0);
        assert!(validate(Some(&caps), Wire::Responses, "any").is_err());
    }
}
