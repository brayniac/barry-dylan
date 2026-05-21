use crate::llm::{LlmClient, LlmError, LlmRequest, LlmResponse};
use async_trait::async_trait;
use serde::Deserialize;

pub struct AnthropicClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: Option<String>,
    model: String,
}

impl AnthropicClient {
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
    content: Vec<ContentBlock>,
    usage: Option<Usage>,
    stop_reason: Option<String>,
}
#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
    input: Option<serde_json::Value>,
}
#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
}

#[async_trait]
impl LlmClient for AnthropicClient {
    async fn complete(&self, req: &LlmRequest) -> Result<LlmResponse, LlmError> {
        crate::llm::retry_transient(|| self.complete_once(req)).await
    }
}

impl AnthropicClient {
    async fn complete_once(&self, req: &LlmRequest) -> Result<LlmResponse, LlmError> {
        let messages: Vec<_> = req
            .messages
            .iter()
            .map(|m| {
                serde_json::json!({
                    "role": match m.role {
                        crate::llm::Role::User => "user",
                        crate::llm::Role::Assistant => "assistant",
                        crate::llm::Role::System => "user", // Anthropic uses top-level system param
                    },
                    "content": m.content,
                })
            })
            .collect();

        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": req.max_tokens,
            "temperature": req.temperature,
            "messages": messages,
        });
        if let Some(sys) = &req.system {
            body["system"] = serde_json::Value::String(sys.clone());
        }
        if let Some(schema) = &req.response_schema {
            body["tools"] = serde_json::json!([{
                "name": "submit",
                "description": "Submit your structured response.",
                "input_schema": schema
            }]);
            body["tool_choice"] = serde_json::json!({"type": "tool", "name": "submit"});
        }

        let url = format!("{}/v1/messages", self.endpoint.trim_end_matches('/'));
        let mut rb = self
            .http
            .post(&url)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body);
        if let Some(k) = &self.api_key {
            rb = rb.header("x-api-key", k);
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
        let text = if req.response_schema.is_some() {
            r.content
                .into_iter()
                .find(|b| b.kind == "tool_use")
                .and_then(|b| b.input)
                .map(|v| v.to_string())
                .ok_or_else(|| LlmError::Shape("no tool_use block in response".into()))?
        } else {
            r.content
                .into_iter()
                .filter(|b| b.kind == "text")
                .map(|b| b.text)
                .collect::<Vec<_>>()
                .join("\n")
        };
        let finish_reason = r.stop_reason.map(|s| match s.as_str() {
            "end_turn" => crate::llm::FinishReason::Stop,
            "max_tokens" => crate::llm::FinishReason::Length,
            other => crate::llm::FinishReason::Other(other.to_string()),
        });
        Ok(LlmResponse {
            text,
            input_tokens: r.usage.as_ref().and_then(|u| u.input_tokens),
            output_tokens: r.usage.as_ref().and_then(|u| u.output_tokens),
            finish_reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{LlmMessage, Role};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn calls_messages_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "k"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [ { "type": "text", "text": "hi" } ],
                "usage": { "input_tokens": 1, "output_tokens": 2 }
            })))
            .mount(&server)
            .await;
        let c = AnthropicClient::new(
            reqwest::Client::new(),
            server.uri(),
            Some("k".into()),
            "model".into(),
        );
        let r = c
            .complete(&LlmRequest {
                system: Some("sys".into()),
                messages: vec![LlmMessage {
                    role: Role::User,
                    content: "go".into(),
                }],
                max_tokens: 64,
                temperature: 0.0,
                response_schema: None,
            })
            .await
            .unwrap();
        assert_eq!(r.text, "hi");
        assert_eq!(r.output_tokens, Some(2));
    }

    #[tokio::test]
    async fn finish_reason_length_when_stop_reason_is_max_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [ { "type": "text", "text": "truncated" } ],
                "stop_reason": "max_tokens",
                "usage": { "input_tokens": 5, "output_tokens": 10 }
            })))
            .mount(&server)
            .await;
        let c = AnthropicClient::new(reqwest::Client::new(), server.uri(), None, "m".into());
        let r = c
            .complete(&LlmRequest {
                system: None,
                messages: vec![LlmMessage {
                    role: Role::User,
                    content: "go".into(),
                }],
                max_tokens: 10,
                temperature: 0.0,
                response_schema: None,
            })
            .await
            .unwrap();
        assert_eq!(r.finish_reason, Some(crate::llm::FinishReason::Length));
    }

    #[tokio::test]
    async fn structured_output_extracts_tool_use_input() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{
                    "type": "tool_use",
                    "id": "tu_123",
                    "name": "submit",
                    "input": { "outcome": "approve", "summary": "LGTM", "findings": [] }
                }],
                "stop_reason": "tool_use",
                "usage": { "input_tokens": 10, "output_tokens": 20 }
            })))
            .mount(&server)
            .await;
        let c = AnthropicClient::new(reqwest::Client::new(), server.uri(), None, "m".into());
        let schema = serde_json::json!({"type": "object"});
        let r = c
            .complete(&LlmRequest {
                system: None,
                messages: vec![LlmMessage {
                    role: Role::User,
                    content: "go".into(),
                }],
                max_tokens: 1024,
                temperature: 0.0,
                response_schema: Some(schema),
            })
            .await
            .unwrap();
        // text should be the JSON-serialized input object
        let parsed: serde_json::Value = serde_json::from_str(&r.text).unwrap();
        assert_eq!(parsed["outcome"], "approve");
        assert_eq!(parsed["summary"], "LGTM");
    }

    #[tokio::test]
    async fn finish_reason_stop_when_stop_reason_is_end_turn() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [ { "type": "text", "text": "done" } ],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 5, "output_tokens": 3 }
            })))
            .mount(&server)
            .await;
        let c = AnthropicClient::new(reqwest::Client::new(), server.uri(), None, "m".into());
        let r = c
            .complete(&LlmRequest {
                system: None,
                messages: vec![LlmMessage {
                    role: Role::User,
                    content: "go".into(),
                }],
                max_tokens: 100,
                temperature: 0.0,
                response_schema: None,
            })
            .await
            .unwrap();
        assert_eq!(r.finish_reason, Some(crate::llm::FinishReason::Stop));
    }
}
