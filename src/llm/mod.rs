//! LLM client abstraction for Anthropic and OpenAI.
//!
//! # Overview
//!
//! Barry Dylan uses LLMs for multi-reviewer functionality:
//! - Two visible reviewers (Barry, Other Barry) with different providers/personas
//! - A hidden judge to determine if reviews materially agree
//!
//! # Architecture
//!
//! ## The `LlmClient` Trait
//!
//! ```ignore
//! pub trait LlmClient: Send + Sync {
//!     async fn complete(&self, req: &LlmRequest) -> Result<LlmResponse, LlmError>;
//! }
//! ```
//!
//! ## Message Format
//!
//! ```ignore
//! pub struct LlmMessage {
//!     pub role: Role,    // System, User, or Assistant
//!     pub content: String,
//! }
//!
//! pub struct LlmRequest {
//!     pub system: Option<String>,  // System prompt
//!     pub messages: Vec<LlmMessage>,
//!     pub max_tokens: u32,
//!     pub temperature: f32,
//! }
//! ```
//!
//! ## Provider Implementations
//!
//! - `anthropic`: Claude models via Anthropic API
//! - `openai`: GPT models via OpenAI API
//!
//! Both implement the `LlmClient` trait with provider-specific request/response
//! shape handling.
//!
//! # Error Handling
//!
//! `LlmError` variants:
//! - `Http(reqwest::Error)`: Network errors
//! - `Api { status, body }`: API errors (4xx/5xx)
//! - `Shape(String)`: Unexpected response format
//!
//! # Retry Logic
//!
//! The `retry_transient` helper handles transient errors with exponential
//! backoff:
//! - Connection errors
//! - Timeout errors
//! - 502-504 status codes
//!
//! Retry policy:
//! - Max 3 attempts
//! - Starting delay: 250ms
//! - Multiplier: 2x
//!
//! ```ignore
//! let result = retry_transient(|| client.complete(&request)).await?;
//! ```
//!
//! # Provider/Endpoint Validation
//!
//! The `factory` module validates that:
//! - Anthropic provider uses Anthropic endpoint
//! - OpenAI provider uses OpenAI-compatible endpoint
//! - Misconfigurations are rejected at startup

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod anthropic;
pub mod factory;
pub mod openai;

/// An LLM message with role and content.
#[derive(Debug, Clone, Serialize)]
pub struct LlmMessage {
    pub role: Role,
    pub content: String,
}

/// Message roles supported by LLMs.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

/// A request to complete an LLM conversation.
#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub system: Option<String>,
    pub messages: Vec<LlmMessage>,
    pub max_tokens: u32,
    pub temperature: f32,
}

/// Response from an LLM.
#[derive(Debug, Clone, Deserialize)]
pub struct LlmResponse {
    pub text: String,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

/// Error from LLM operations.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("api {status}: {body}")]
    Api { status: u16, body: String },
    #[error("unexpected response shape: {0}")]
    Shape(String),
}

/// Trait implemented by all LLM clients.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Complete the given LLM request and return the response.
    async fn complete(&self, req: &LlmRequest) -> Result<LlmResponse, LlmError>;

    /// Optional: name of the client for logging purposes.
    fn name(&self) -> &'static str {
        "llm_client"
    }
}

fn is_transient(e: &LlmError) -> bool {
    match e {
        LlmError::Http(err) => err.is_connect() || err.is_timeout(),
        LlmError::Api { status, .. } => matches!(*status, 502..=504),
        LlmError::Shape(_) => false,
    }
}

/// Retry a closure with exponential backoff on transient errors.
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

/// A wrapper client that adds timing metrics for LLM calls.
pub struct TimedClient<C: LlmClient + Send + Sync> {
    inner: C,
    name: &'static str,
}

impl<C: LlmClient + Send + Sync> TimedClient<C> {
    pub fn new(inner: C) -> Self {
        let name = inner.name();
        Self { inner, name }
    }

    pub fn with_name(inner: C, name: &'static str) -> Self {
        Self { inner, name }
    }
}

#[async_trait]
impl<C: LlmClient + Send + Sync> LlmClient for TimedClient<C> {
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
        self.name
    }
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
