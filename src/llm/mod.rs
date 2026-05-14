use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod anthropic;
pub mod factory;
pub mod openai;

#[derive(Debug, Clone, Serialize)]
pub struct LlmMessage {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub system: Option<String>,
    pub messages: Vec<LlmMessage>,
    pub max_tokens: u32,
    pub temperature: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LlmResponse {
    pub text: String,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("api {status}: {body}")]
    Api { status: u16, body: String },
    #[error("unexpected response shape: {0}")]
    Shape(String),
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(&self, req: &LlmRequest) -> Result<LlmResponse, LlmError>;
}

fn is_transient(e: &LlmError) -> bool {
    match e {
        LlmError::Http(err) => err.is_connect() || err.is_timeout(),
        LlmError::Api { status, .. } => matches!(*status, 502..=504),
        LlmError::Shape(_) => false,
    }
}

pub(crate) async fn retry_transient<F, Fut>(mut f: F) -> Result<LlmResponse, LlmError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<LlmResponse, LlmError>>,
{
    let mut delay = std::time::Duration::from_millis(250);
    let max_attempts = 3u32;
    for attempt in 0..max_attempts {
        match f().await {
            Ok(r) => return Ok(r),
            Err(e) if attempt + 1 < max_attempts && is_transient(&e) => {
                tracing::warn!(error = ?e, attempt = attempt + 1, "llm transient error; retrying");
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2);
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_transient_classifies_api_status() {
        for code in [500u16, 501, 504, 502, 503] {
            let e = LlmError::Api {
                status: code,
                body: "x".into(),
            };
            let want = matches!(code, 502..=504);
            assert_eq!(is_transient(&e), want, "status {code}");
        }
    }

    #[test]
    fn is_transient_rejects_shape() {
        assert!(!is_transient(&LlmError::Shape("bad json".into())));
    }
}
