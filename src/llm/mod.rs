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
