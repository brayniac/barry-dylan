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
//!     pub temperature: Option<f32>,
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
//! - 502-504 status codes
//!
//! A request that timed out is not retried.
//!
//! Retry policy:
//! - Up to 7 attempts, waiting 1, 2, 4, 8, 16 and 16 s between them (47 s
//!   of waiting in all, plus the attempts themselves), which outlasts a local llama-server restarting: it refuses
//!   connections, then answers 503 while it loads, for about 25 s.
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
use serde::Serialize;

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

#[derive(Debug, Clone, PartialEq)]
pub enum FinishReason {
    Stop,
    Length,
    Other(String),
}

impl std::fmt::Display for FinishReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FinishReason::Stop => write!(f, "stop"),
            FinishReason::Length => write!(f, "length"),
            FinishReason::Other(s) => write!(f, "other:{s}"),
        }
    }
}

/// A request to complete an LLM conversation.
#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub system: Option<String>,
    pub messages: Vec<LlmMessage>,
    pub max_tokens: u32,
    /// Sampling temperature, or `None` to leave it to the endpoint. For a
    /// local llama-server that means the model's own recommendation, read
    /// from the GGUF (`general.sampling.*`); Qwen's thinking models ship
    /// 1.0 / top_p 0.95 / top_k 20 and document that greedy decoding
    /// degrades them and can loop. barry pinned 0.0 everywhere until
    /// 2026-09-18.
    pub temperature: Option<f32>,
    pub response_schema: Option<serde_json::Value>,
}

/// Response from an LLM.
#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub text: String,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub finish_reason: Option<FinishReason>,
}

/// Error from LLM operations.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// The URL is stripped on conversion, since this text reaches check runs
    /// on public repositories. Only this variant is scrubbed: an `Api` body
    /// is the server's own text and is published as is.
    #[error("http: {0}")]
    Http(reqwest::Error),
    #[error("api {status}: {body}")]
    Api { status: u16, body: String },
    #[error("unexpected response shape: {0}")]
    Shape(String),
    /// The model answered with no content at all. Distinct from `Shape`
    /// because the fix is different: a thinking model given too small a
    /// budget spends it all thinking, and the answer is a bigger budget or
    /// a smaller prompt, not a parser.
    #[error("model returned no content (finish reason {finish}, {output_tokens} output tokens)")]
    Empty { finish: String, output_tokens: u32 },
}

impl From<reqwest::Error> for LlmError {
    fn from(e: reqwest::Error) -> Self {
        LlmError::Http(e.without_url())
    }
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
        // Not a timeout: the request ran for `request_timeout_secs` and the
        // server dropped it, so a retry costs that long again.
        LlmError::Http(err) => err.is_connect(),
        LlmError::Api { status, .. } => matches!(*status, 502..=504),
        LlmError::Shape(_) => false,
        LlmError::Empty { .. } => false,
    }
}

/// The waits between attempts in [`retry_transient`], in seconds.
const RETRY_DELAYS_SECS: [u64; 6] = [1, 2, 4, 8, 16, 16];

/// Retry a closure with exponential backoff on transient errors.
pub(crate) async fn retry_transient<F, Fut>(mut f: F) -> Result<LlmResponse, LlmError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<LlmResponse, LlmError>>,
{
    let mut delays = RETRY_DELAYS_SECS.iter();
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match f().await {
            Ok(r) => return Ok(r),
            Err(e) if is_transient(&e) => match delays.next() {
                Some(secs) => {
                    tracing::warn!(error = ?e, attempt, retry_in_secs = secs, "llm transient error; retrying");
                    tokio::time::sleep(std::time::Duration::from_secs(*secs)).await;
                }
                None => return Err(e),
            },
            Err(e) => return Err(e),
        }
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

    fn unavailable() -> LlmError {
        LlmError::Api {
            status: 503,
            body: "Loading model".into(),
        }
    }

    /// A llama-server restart answers 503 for about 25 s; the retries must
    /// outlast it.
    #[tokio::test(start_paused = true)]
    async fn transient_errors_are_retried_for_47_seconds() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let started = tokio::time::Instant::now();
        let r = retry_transient(|| async {
            if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 6 {
                Err(unavailable())
            } else {
                Ok(LlmResponse {
                    text: "ok".into(),
                    input_tokens: None,
                    output_tokens: None,
                    finish_reason: None,
                })
            }
        })
        .await;
        assert_eq!(r.unwrap().text, "ok");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 7);
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(47));
    }

    #[tokio::test(start_paused = true)]
    async fn a_seventh_transient_error_is_returned() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let r = retry_transient(|| async {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(unavailable())
        })
        .await;
        assert!(matches!(r, Err(LlmError::Api { status: 503, .. })));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 7);
    }

    #[test]
    fn is_transient_rejects_shape() {
        assert!(!is_transient(&LlmError::Shape("bad json".into())));
    }

    #[test]
    fn llm_response_has_finish_reason_field() {
        let r = LlmResponse {
            text: "hi".into(),
            input_tokens: None,
            output_tokens: None,
            finish_reason: Some(FinishReason::Length),
        };
        assert!(matches!(r.finish_reason, Some(FinishReason::Length)));
    }

    #[test]
    fn llm_request_has_response_schema_field() {
        let schema = serde_json::json!({"type": "object"});
        let req = LlmRequest {
            system: None,
            messages: vec![],
            max_tokens: 100,
            temperature: None,
            response_schema: Some(schema.clone()),
        };
        assert_eq!(req.response_schema, Some(schema));
    }
}
