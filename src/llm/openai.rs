use crate::llm::{LlmClient, LlmError, LlmRequest, LlmResponse};
use async_trait::async_trait;
use serde::Deserialize;

pub struct OpenAiClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: Option<String>,
    model: String,
}

impl OpenAiClient {
    pub fn new(
        http: reqwest::Client,
        endpoint: String,
        api_key: Option<String>,
        model: String,
    ) -> Self {
        Self {
            http,
            endpoint,
            api_key,
            model,
        }
    }
}

#[derive(Deserialize)]
struct Resp {
    choices: Vec<Choice>,
    usage: Option<Usage>,
}
#[derive(Deserialize)]
struct Choice {
    message: Msg,
}
#[derive(Deserialize)]
struct Msg {
    content: String,
}
#[derive(Deserialize)]
struct Usage {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
}

#[async_trait]
impl LlmClient for OpenAiClient {
    async fn complete(&self, req: &LlmRequest) -> Result<LlmResponse, LlmError> {
        crate::llm::retry_transient(|| self.complete_once(req)).await
    }
}

impl OpenAiClient {
    async fn complete_once(&self, req: &LlmRequest) -> Result<LlmResponse, LlmError> {
        let mut messages = Vec::new();
        if let Some(sys) = &req.system {
            messages.push(serde_json::json!({ "role": "system", "content": sys }));
        }
        for m in &req.messages {
            messages.push(serde_json::json!({
                "role": match m.role {
                    crate::llm::Role::User => "user",
                    crate::llm::Role::Assistant => "assistant",
                    crate::llm::Role::System => "system",
                },
                "content": m.content,
            }));
        }
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": req.max_tokens,
            "temperature": req.temperature,
            "messages": messages,
            "cache_prompt": true,
        });
        let url = format!("{}/chat/completions", self.endpoint.trim_end_matches('/'));
        let mut rb = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .json(&body);
        if let Some(k) = &self.api_key {
            rb = rb.bearer_auth(k);
        }
        let resp = rb.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::Api {
                status: status.as_u16(),
                body,
            });
        }
        let r: Resp = resp.json().await?;
        let text = r
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| LlmError::Shape("no choices".into()))?;
        Ok(LlmResponse {
            text,
            input_tokens: r.usage.as_ref().and_then(|u| u.prompt_tokens),
            output_tokens: r.usage.as_ref().and_then(|u| u.completion_tokens),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{LlmMessage, Role};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn retries_after_transient_5xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [ { "message": { "content": "ok" } } ]
            })))
            .mount(&server)
            .await;
        let c = OpenAiClient::new(reqwest::Client::new(), server.uri(), None, "m".into());
        let r = c
            .complete(&LlmRequest {
                system: None,
                messages: vec![LlmMessage {
                    role: Role::User,
                    content: "q".into(),
                }],
                max_tokens: 32,
                temperature: 0.0,
            })
            .await
            .unwrap();
        assert_eq!(r.text, "ok");
    }

    #[tokio::test]
    async fn does_not_retry_4xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(400))
            .expect(1)
            .mount(&server)
            .await;
        let c = OpenAiClient::new(reqwest::Client::new(), server.uri(), None, "m".into());
        let e = c
            .complete(&LlmRequest {
                system: None,
                messages: vec![LlmMessage {
                    role: Role::User,
                    content: "q".into(),
                }],
                max_tokens: 32,
                temperature: 0.0,
            })
            .await
            .err()
            .unwrap();
        assert!(matches!(e, LlmError::Api { status: 400, .. }));
    }

    #[tokio::test]
    async fn unauthed_local_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [ { "message": { "content": "ok" } } ]
            })))
            .mount(&server)
            .await;
        let c = OpenAiClient::new(reqwest::Client::new(), server.uri(), None, "m".into());
        let r = c
            .complete(&LlmRequest {
                system: None,
                messages: vec![LlmMessage {
                    role: Role::User,
                    content: "q".into(),
                }],
                max_tokens: 32,
                temperature: 0.0,
            })
            .await
            .unwrap();
        assert_eq!(r.text, "ok");
    }
}
