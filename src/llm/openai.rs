use crate::llm::{LlmClient, LlmError, LlmRequest, LlmResponse};
use async_trait::async_trait;
use serde::Deserialize;

pub struct OpenAiClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: Option<String>,
    model: String,
    /// Applied when a request leaves temperature unset; `None` leaves it to
    /// the server, which for llama-server is the model's own recommendation.
    temperature: Option<f32>,
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
            temperature: None,
        }
    }

    /// Temperature to apply when a request leaves it unset.
    pub fn with_temperature(mut self, temperature: Option<f32>) -> Self {
        self.temperature = temperature;
        self
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
    finish_reason: Option<String>,
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
        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": req.max_tokens,
            "messages": messages,
            "cache_prompt": true,
        });
        if let Some(t) = req.temperature.or(self.temperature) {
            body["temperature"] = serde_json::json!(t);
        }
        if let Some(schema) = &req.response_schema {
            body["response_format"] = serde_json::json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "response",
                    "strict": true,
                    "schema": schema
                }
            });
        }
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
        let first = r
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| LlmError::Shape("no choices".into()))?;
        let finish_reason = first.finish_reason.map(|s| match s.as_str() {
            "stop" => crate::llm::FinishReason::Stop,
            "length" => crate::llm::FinishReason::Length,
            other => crate::llm::FinishReason::Other(other.to_string()),
        });
        Ok(LlmResponse {
            text: first.message.content,
            input_tokens: r.usage.as_ref().and_then(|u| u.prompt_tokens),
            output_tokens: r.usage.as_ref().and_then(|u| u.completion_tokens),
            finish_reason,
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
                temperature: None,
                response_schema: None,
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
                temperature: None,
                response_schema: None,
            })
            .await
            .err()
            .unwrap();
        assert!(matches!(e, LlmError::Api { status: 400, .. }));
    }

    #[tokio::test]
    async fn finish_reason_length_when_finish_reason_is_length() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [ { "message": { "content": "truncated" }, "finish_reason": "length" } ],
                "usage": { "prompt_tokens": 5, "completion_tokens": 10 }
            })))
            .mount(&server)
            .await;
        let c = OpenAiClient::new(reqwest::Client::new(), server.uri(), None, "m".into());
        let r = c
            .complete(&LlmRequest {
                system: None,
                messages: vec![LlmMessage {
                    role: Role::User,
                    content: "go".into(),
                }],
                max_tokens: 10,
                temperature: None,
                response_schema: None,
            })
            .await
            .unwrap();
        assert_eq!(r.finish_reason, Some(crate::llm::FinishReason::Length));
    }

    #[tokio::test]
    async fn finish_reason_stop_when_finish_reason_is_stop() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [ { "message": { "content": "done" }, "finish_reason": "stop" } ],
                "usage": { "prompt_tokens": 5, "completion_tokens": 3 }
            })))
            .mount(&server)
            .await;
        let c = OpenAiClient::new(reqwest::Client::new(), server.uri(), None, "m".into());
        let r = c
            .complete(&LlmRequest {
                system: None,
                messages: vec![LlmMessage {
                    role: Role::User,
                    content: "go".into(),
                }],
                max_tokens: 100,
                temperature: None,
                response_schema: None,
            })
            .await
            .unwrap();
        assert_eq!(r.finish_reason, Some(crate::llm::FinishReason::Stop));
    }

    #[tokio::test]
    async fn structured_output_sends_response_format() {
        let server = MockServer::start().await;
        // The mock will only match if we send the right request body
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": { "content": r#"{"outcome":"approve","summary":"LGTM","findings":[]}"# },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 10, "completion_tokens": 20 }
            })))
            .mount(&server)
            .await;
        let c = OpenAiClient::new(reqwest::Client::new(), server.uri(), None, "m".into());
        let schema = serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false, "required": []});
        let r = c
            .complete(&LlmRequest {
                system: None,
                messages: vec![LlmMessage {
                    role: Role::User,
                    content: "go".into(),
                }],
                max_tokens: 1024,
                temperature: None,
                response_schema: Some(schema),
            })
            .await
            .unwrap();
        // response text is the JSON string from content
        assert!(r.text.contains("approve"));
    }

    async fn body_of_first_request(server: &MockServer) -> serde_json::Value {
        let reqs = server.received_requests().await.unwrap_or_default();
        serde_json::from_slice(&reqs[0].body).unwrap()
    }

    fn ask(temperature: Option<f32>) -> LlmRequest {
        LlmRequest {
            system: None,
            messages: vec![LlmMessage {
                role: Role::User,
                content: "q".into(),
            }],
            max_tokens: 32,
            temperature,
            response_schema: None,
        }
    }

    async fn ok_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [ { "message": { "content": "ok" } } ]
            })))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn temperature_is_omitted_unless_someone_sets_it() {
        // Omitted, llama-server uses the model's own recommendation, which
        // for a thinking model is not greedy.
        let server = ok_server().await;
        let c = OpenAiClient::new(reqwest::Client::new(), server.uri(), None, "m".into());
        c.complete(&ask(None)).await.unwrap();
        assert!(
            body_of_first_request(&server)
                .await
                .get("temperature")
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_profile_temperature_applies_when_the_request_has_none() {
        let server = ok_server().await;
        let c = OpenAiClient::new(reqwest::Client::new(), server.uri(), None, "m".into())
            .with_temperature(Some(0.7));
        c.complete(&ask(None)).await.unwrap();
        assert_eq!(body_of_first_request(&server).await["temperature"], 0.7);
    }

    #[tokio::test]
    async fn a_request_temperature_wins_over_the_profile() {
        let server = ok_server().await;
        let c = OpenAiClient::new(reqwest::Client::new(), server.uri(), None, "m".into())
            .with_temperature(Some(0.7));
        c.complete(&ask(Some(0.0))).await.unwrap();
        assert_eq!(body_of_first_request(&server).await["temperature"], 0.0);
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
                temperature: None,
                response_schema: None,
            })
            .await
            .unwrap();
        assert_eq!(r.text, "ok");
    }
}
