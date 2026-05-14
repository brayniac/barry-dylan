use crate::config::{LlmProviderKind, LlmProfile};
use crate::llm::{LlmClient, LlmError, LlmRequest, LlmResponse};
use crate::llm::anthropic::AnthropicClient;
use crate::llm::openai::OpenAiClient;
use std::sync::Arc;
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum FactoryError {
    #[error("missing required env var {0}")]
    MissingEnv(String),
    #[error("invalid endpoint URL: {0}")]
    BadUrl(String),
    #[error("safety guard: provider=anthropic but endpoint host {host:?} is not api.anthropic.com")]
    EndpointMismatch { host: String },
}

pub fn build(
    profile: &LlmProfile,
    http: reqwest::Client,
) -> Result<Arc<dyn LlmClient>, FactoryError> {
    build_named(profile, http, &profile_to_string(profile.provider))
}

/// Convert LlmProviderKind to a string for naming clients.
fn profile_to_string(provider: LlmProviderKind) -> String {
    match provider {
        LlmProviderKind::Anthropic => "anthropic".to_string(),
        LlmProviderKind::Openai => "openai".to_string(),
    }
}

/// Build an LLM client with a custom name for logging.
pub fn build_named(
    profile: &LlmProfile,
    http: reqwest::Client,
    name: &str,
) -> Result<Arc<dyn LlmClient>, FactoryError> {
    let api_key = match &profile.api_key_env {
        Some(name) if !name.is_empty() => {
            Some(std::env::var(name).map_err(|_| FactoryError::MissingEnv(name.clone()))?)
        }
        _ => None,
    };
    let url = Url::parse(&profile.endpoint).map_err(|e| FactoryError::BadUrl(e.to_string()))?;
    let host = url.host_str().unwrap_or("").to_string();

    let client: Arc<dyn LlmClient> = match profile.provider {
        LlmProviderKind::Anthropic => {
            if host != "api.anthropic.com" {
                return Err(FactoryError::EndpointMismatch { host });
            }
            Arc::new(AnthropicClient::new(
                http,
                profile.endpoint.clone(),
                api_key,
                profile.model.clone(),
            ))
        }
        LlmProviderKind::Openai => Arc::new(OpenAiClient::new(
            http,
            profile.endpoint.clone(),
            api_key,
            profile.model.clone(),
        )),
    };

    // Return the client wrapped with timing
    Ok(Arc::new(TimedClient::new(client, name)))
}

/// A wrapper client that adds timing metrics for LLM calls, working with trait objects.
pub struct TimedClient {
    inner: Arc<dyn LlmClient>,
    name: String,
}

impl TimedClient {
    pub fn new(inner: Arc<dyn LlmClient>, name: &str) -> Self {
        Self {
            inner,
            name: name.to_string(),
        }
    }
}

impl std::fmt::Debug for TimedClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimedClient")
            .field("name", &self.name)
            .finish()
    }
}

#[async_trait::async_trait]
impl LlmClient for TimedClient {
    async fn complete(&self, req: &LlmRequest) -> Result<LlmResponse, LlmError> {
        let span = tracing::info_span!(
            "llm.call",
            client = self.name,
            max_tokens = req.max_tokens,
            messages = req.messages.len()
        );
        let _enter = span.enter();

        let start = std::time::Instant::now();
        let result = self.inner.complete(req).await;
        let duration_ms = start.elapsed().as_millis() as u64;

        match &result {
            Ok(resp) => {
                tracing::info!(
                    duration_ms,
                    input_tokens = resp.input_tokens,
                    output_tokens = resp.output_tokens,
                    "llm call completed"
                );
            }
            Err(e) => {
                tracing::warn!(
                    duration_ms,
                    error = ?e,
                    "llm call failed"
                );
            }
        }

        result
    }

    fn name(&self) -> &'static str {
        "timed_llm"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LlmProfile;

    fn profile(provider: LlmProviderKind, endpoint: &str) -> LlmProfile {
        LlmProfile {
            provider,
            endpoint: endpoint.into(),
            api_key_env: None,
            model: "m".into(),
            max_tokens: 1024,
            request_timeout_secs: 60,
        }
    }

    #[test]
    fn anthropic_with_non_anthropic_endpoint_rejected() {
        let p = profile(LlmProviderKind::Anthropic, "https://example.com");
        let result = build(&p, reqwest::Client::new());
        let err = result.err().expect("expected an error");
        assert!(matches!(err, FactoryError::EndpointMismatch { .. }));
    }

    #[test]
    fn openai_with_local_endpoint_ok() {
        let p = profile(LlmProviderKind::Openai, "http://localhost:1234/v1");
        let _ = build(&p, reqwest::Client::new()).unwrap();
    }
}
