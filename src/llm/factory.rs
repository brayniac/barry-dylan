use crate::config::{LlmProfile, LlmProviderKind};
use crate::llm::LlmClient;
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
    let api_key = match &profile.api_key_env {
        Some(name) if !name.is_empty() => {
            Some(std::env::var(name).map_err(|_| FactoryError::MissingEnv(name.clone()))?)
        }
        _ => None,
    };
    let url = Url::parse(&profile.endpoint).map_err(|e| FactoryError::BadUrl(e.to_string()))?;
    let host = url.host_str().unwrap_or("").to_string();

    match profile.provider {
        LlmProviderKind::Anthropic => {
            if host != "api.anthropic.com" {
                return Err(FactoryError::EndpointMismatch { host });
            }
            Ok(Arc::new(AnthropicClient::new(
                http,
                profile.endpoint.clone(),
                api_key,
                profile.model.clone(),
            )))
        }
        LlmProviderKind::Openai => Ok(Arc::new(OpenAiClient::new(
            http,
            profile.endpoint.clone(),
            api_key,
            profile.model.clone(),
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LlmProfile, LlmProviderKind};

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
