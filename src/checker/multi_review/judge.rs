use super::parse_util::locate_json;
use crate::checker::multi_review::review::UnifiedReview;
use crate::checker::multi_review::synthesis::TokenCount;
use crate::llm::{LlmClient, LlmError, LlmMessage, LlmRequest, Role};
use serde::Deserialize;

const JUDGE_TEMPLATE: &str = include_str!("prompts/judge.md");

#[derive(Debug, thiserror::Error)]
pub enum JudgeError {
    #[error("llm: {0}")]
    Llm(#[from] LlmError),
    #[error("could not parse judge output: {0}")]
    Parse(String),
}

#[derive(Debug, Deserialize)]
struct JudgeResp {
    agree: bool,
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Clone)]
pub struct JudgeVerdict {
    pub agree: bool,
    pub reason: String,
    pub tokens: TokenCount,
}

pub async fn judge(
    client: &dyn LlmClient,
    barry: &UnifiedReview,
    other: &UnifiedReview,
    max_tokens: u32,
) -> Result<JudgeVerdict, JudgeError> {
    let user = format!(
        "=== Barry's review ===\n{}\n\n=== Other Barry's review ===\n{}",
        serde_json::to_string(&serde_json::json!({
            "outcome": barry.outcome,
            "summary": barry.summary,
            "findings": barry.findings.iter().map(|f| serde_json::json!({
                "file": f.file, "line": f.line, "message": f.message
            })).collect::<Vec<_>>(),
        }))
        .unwrap_or_default(),
        serde_json::to_string(&serde_json::json!({
            "outcome": other.outcome,
            "summary": other.summary,
            "findings": other.findings.iter().map(|f| serde_json::json!({
                "file": f.file, "line": f.line, "message": f.message
            })).collect::<Vec<_>>(),
        }))
        .unwrap_or_default(),
    );
    match judge_once(client, &user, max_tokens).await {
        Ok(v) => Ok(v),
        Err(JudgeError::Parse(raw)) => {
            let truncated: String = raw.chars().take(2048).collect();
            tracing::warn!(raw_text = %truncated, "judge parse failed; retrying once");
            match judge_once(client, &user, max_tokens).await {
                Ok(v) => Ok(v),
                Err(JudgeError::Parse(raw2)) => {
                    let truncated: String = raw2.chars().take(2048).collect();
                    tracing::warn!(raw_text = %truncated, "judge parse failed on retry; giving up");
                    Err(JudgeError::Parse(raw2))
                }
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

async fn judge_once(
    client: &dyn LlmClient,
    user: &str,
    max_tokens: u32,
) -> Result<JudgeVerdict, JudgeError> {
    let req = LlmRequest {
        system: Some(JUDGE_TEMPLATE.to_string()),
        messages: vec![LlmMessage {
            role: Role::User,
            content: user.to_string(),
        }],
        max_tokens,
        temperature: 0.0,
    };
    let resp = client.complete(&req).await?;
    let slice = locate_json(&resp.text).ok_or_else(|| JudgeError::Parse(resp.text.clone()))?;
    let parsed: JudgeResp =
        serde_json::from_str(slice).map_err(|e| JudgeError::Parse(e.to_string()))?;
    Ok(JudgeVerdict {
        agree: parsed.agree,
        reason: parsed.reason,
        tokens: TokenCount {
            input: u64::from(resp.input_tokens.unwrap_or(0)),
            output: u64::from(resp.output_tokens.unwrap_or(0)),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checker::multi_review::review::{Outcome, UnifiedReview};
    use crate::llm::{LlmClient, LlmError, LlmRequest, LlmResponse};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    struct StubClient {
        resp: String,
        recorded: Arc<Mutex<Vec<LlmRequest>>>,
    }
    #[async_trait]
    impl LlmClient for StubClient {
        async fn complete(&self, req: &LlmRequest) -> Result<LlmResponse, LlmError> {
            self.recorded.lock().unwrap().push(req.clone());
            Ok(LlmResponse {
                text: self.resp.clone(),
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
            })
        }
    }

    fn r(outcome: Outcome, summary: &str) -> UnifiedReview {
        UnifiedReview {
            outcome,
            summary: summary.into(),
            findings: vec![],
        }
    }

    #[tokio::test]
    async fn parses_agree() {
        let rec = Arc::new(Mutex::new(vec![]));
        let c = StubClient {
            resp: r#"{"agree":true,"reason":"same"}"#.into(),
            recorded: rec,
        };
        let v = judge(
            &c,
            &r(Outcome::Approve, "x"),
            &r(Outcome::Approve, "y"),
            256,
        )
        .await
        .unwrap();
        assert!(v.agree);
    }

    #[tokio::test]
    async fn parses_disagree() {
        let rec = Arc::new(Mutex::new(vec![]));
        let c = StubClient {
            resp: r#"{"agree":false,"reason":"diff outcomes"}"#.into(),
            recorded: rec,
        };
        let v = judge(
            &c,
            &r(Outcome::Approve, "x"),
            &r(Outcome::RequestChanges, "y"),
            256,
        )
        .await
        .unwrap();
        assert!(!v.agree);
        assert_eq!(v.reason, "diff outcomes");
    }

    #[tokio::test]
    async fn includes_both_reviews_in_prompt() {
        let rec = Arc::new(Mutex::new(vec![]));
        let c = StubClient {
            resp: r#"{"agree":true,"reason":""}"#.into(),
            recorded: rec.clone(),
        };
        let _ = judge(
            &c,
            &r(Outcome::Approve, "barry-says-this"),
            &r(Outcome::Approve, "ob-says-that"),
            256,
        )
        .await
        .unwrap();
        let r = rec.lock().unwrap();
        let user = &r[0].messages[0].content;
        assert!(user.contains("barry-says-this"));
        assert!(user.contains("ob-says-that"));
        assert!(user.contains("Barry"));
        assert!(user.contains("Other Barry"));
    }

    #[tokio::test]
    async fn errors_on_unparseable() {
        let rec = Arc::new(Mutex::new(vec![]));
        let c = StubClient {
            resp: "no json here".into(),
            recorded: rec,
        };
        let err = judge(
            &c,
            &r(Outcome::Approve, "x"),
            &r(Outcome::Approve, "y"),
            256,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, JudgeError::Parse(_)));
    }

    struct ScriptClient {
        resps: Mutex<Vec<Result<LlmResponse, crate::llm::LlmError>>>,
        recorded: Arc<Mutex<Vec<LlmRequest>>>,
    }

    #[async_trait]
    impl crate::llm::LlmClient for ScriptClient {
        async fn complete(&self, req: &LlmRequest) -> Result<LlmResponse, crate::llm::LlmError> {
            self.recorded.lock().unwrap().push(req.clone());
            let mut q = self.resps.lock().unwrap();
            if q.is_empty() {
                panic!("ScriptClient exhausted");
            }
            q.remove(0)
        }
    }

    fn rev() -> UnifiedReview {
        UnifiedReview {
            outcome: Outcome::Comment,
            summary: "x".into(),
            findings: vec![],
        }
    }

    #[tokio::test]
    async fn judge_retries_once_on_parse_then_succeeds() {
        let recorded = Arc::new(Mutex::new(vec![]));
        let client = ScriptClient {
            resps: Mutex::new(vec![
                Ok(LlmResponse {
                    text: "".into(),
                    input_tokens: Some(10),
                    output_tokens: Some(0),
                    finish_reason: None,
                }),
                Ok(LlmResponse {
                    text: r#"{"agree":true,"reason":"ok"}"#.into(),
                    input_tokens: Some(11),
                    output_tokens: Some(5),
                    finish_reason: None,
                }),
            ]),
            recorded: recorded.clone(),
        };
        let v = judge(&client, &rev(), &rev(), 256).await.unwrap();
        assert!(v.agree);
        // Second call (the successful one) is the one whose tokens we credit.
        assert_eq!(v.tokens.input, 11);
        assert_eq!(v.tokens.output, 5);
        assert_eq!(recorded.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn judge_returns_parse_after_both_attempts_fail() {
        let recorded = Arc::new(Mutex::new(vec![]));
        let client = ScriptClient {
            resps: Mutex::new(vec![
                Ok(LlmResponse {
                    text: "".into(),
                    input_tokens: Some(1),
                    output_tokens: Some(0),
                    finish_reason: None,
                }),
                Ok(LlmResponse {
                    text: "still garbage".into(),
                    input_tokens: Some(1),
                    output_tokens: Some(0),
                    finish_reason: None,
                }),
            ]),
            recorded: recorded.clone(),
        };
        let err = judge(&client, &rev(), &rev(), 256).await.unwrap_err();
        assert!(matches!(err, JudgeError::Parse(_)));
        assert_eq!(recorded.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn judge_does_not_retry_on_transport_errors() {
        let recorded = Arc::new(Mutex::new(vec![]));
        let client = ScriptClient {
            resps: Mutex::new(vec![Err(crate::llm::LlmError::Api {
                status: 500,
                body: "boom".into(),
            })]),
            recorded: recorded.clone(),
        };
        let err = judge(&client, &rev(), &rev(), 256).await.unwrap_err();
        assert!(matches!(err, JudgeError::Llm(_)));
        assert_eq!(recorded.lock().unwrap().len(), 1);
    }
}
