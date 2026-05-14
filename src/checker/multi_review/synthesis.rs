use crate::checker::multi_review::persona::Persona;
use crate::checker::multi_review::review::{ParseError, UnifiedReview, parse};
use crate::github::pr::ChangedFile;
use crate::llm::{LlmClient, LlmError, LlmMessage, LlmRequest, Role};

const SYNTHESIS_TEMPLATE: &str = include_str!("prompts/synthesis.md");

#[derive(Debug, thiserror::Error)]
pub enum SynthesisError {
    #[error("llm: {0}")]
    Llm(#[from] LlmError),
    #[error("parse: {0}")]
    Parse(#[from] ParseError),
}

pub struct PersonaDraft {
    pub persona: &'static str,
    pub raw: String,
}

/// Run a single persona prompt over the diff and return its raw text output.
pub async fn run_persona(
    client: &dyn LlmClient,
    persona: &Persona,
    diff_block: &str,
    max_tokens: u32,
) -> Result<PersonaDraft, LlmError> {
    let req = LlmRequest {
        system: Some(persona.prompt.as_ref().clone()),
        messages: vec![LlmMessage {
            role: Role::User,
            content: format!(
                "{diff_block}\n\nReturn ONLY a JSON object with: \
                 {{\"findings\":[{{\"file\":\"<path>\",\"line\":<int>,\"message\":\"<text>\"}}],\
                 \"summary\":\"<one short sentence>\"}}"
            ),
        }],
        max_tokens,
        temperature: 0.0,
    };
    let resp = client.complete(&req).await?;
    Ok(PersonaDraft {
        persona: persona.name,
        raw: resp.text,
    })
}

/// Run synthesis over N persona drafts and return a parsed UnifiedReview.
pub async fn synthesize(
    client: &dyn LlmClient,
    drafts: &[PersonaDraft],
    diff_block: &str,
    prior_peer_review: Option<&str>,
    max_tokens: u32,
) -> Result<UnifiedReview, SynthesisError> {
    let mut user = String::new();
    user.push_str(diff_block);
    user.push_str("\n\n=== persona drafts ===\n");
    for d in drafts {
        user.push_str(&format!("--- {} ---\n{}\n", d.persona, d.raw));
    }
    if let Some(peer) = prior_peer_review {
        user.push_str("\n=== peer review (R1 from the other Barry) ===\n");
        user.push_str(peer);
        user.push_str("\nYou MAY revise your position based on the peer review. If you do, say so in summary.\n");
    }
    let req = LlmRequest {
        system: Some(SYNTHESIS_TEMPLATE.to_string()),
        messages: vec![LlmMessage {
            role: Role::User,
            content: user,
        }],
        max_tokens,
        temperature: 0.0,
    };
    let resp = client.complete(&req).await?;
    Ok(parse(&resp.text)?)
}

/// Render a diff block from changed files, suitable for embedding in a user message.
pub fn render_diff_block(files: &[ChangedFile]) -> String {
    let mut s = String::from("=== diff begins ===\n");
    for f in files {
        s.push_str(&format!("File: {}\n```\n", f.filename));
        if let Some(p) = &f.patch {
            s.push_str(p);
        }
        s.push_str("\n```\n");
    }
    s.push_str("=== diff ends ===\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::LlmResponse;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    struct StubClient {
        responses: Mutex<Vec<String>>,
        recorded: Arc<Mutex<Vec<LlmRequest>>>,
    }
    #[async_trait]
    impl LlmClient for StubClient {
        async fn complete(&self, req: &LlmRequest) -> Result<LlmResponse, LlmError> {
            self.recorded.lock().unwrap().push(req.clone());
            let text = self.responses.lock().unwrap().remove(0);
            Ok(LlmResponse {
                text,
                input_tokens: None,
                output_tokens: None,
            })
        }
    }

    fn persona(name: &'static str) -> Persona {
        Persona {
            name,
            prompt: Arc::new(format!("you are {name}")),
        }
    }

    fn file(name: &str, patch: &str) -> ChangedFile {
        ChangedFile {
            filename: name.into(),
            status: "modified".into(),
            additions: 1,
            deletions: 0,
            changes: 1,
            patch: Some(patch.into()),
        }
    }

    #[tokio::test]
    async fn run_persona_passes_prompt_as_system() {
        let recorded = Arc::new(Mutex::new(vec![]));
        let client = StubClient {
            responses: Mutex::new(vec![r#"{"findings":[],"summary":"ok"}"#.into()]),
            recorded: recorded.clone(),
        };
        let p = persona("security");
        let d = run_persona(&client, &p, "diff", 1024).await.unwrap();
        assert_eq!(d.persona, "security");
        let r = recorded.lock().unwrap();
        assert!(r[0].system.as_ref().unwrap().contains("you are security"));
    }

    #[tokio::test]
    async fn synthesize_includes_all_persona_drafts_in_user() {
        let recorded = Arc::new(Mutex::new(vec![]));
        let client = StubClient {
            responses: Mutex::new(vec![
                r#"{"outcome":"approve","summary":"LGTM","findings":[]}"#.into(),
            ]),
            recorded: recorded.clone(),
        };
        let drafts = vec![
            PersonaDraft { persona: "security", raw: "sec-draft".into() },
            PersonaDraft { persona: "style", raw: "style-draft".into() },
        ];
        let r = synthesize(&client, &drafts, "diff", None, 1024).await.unwrap();
        assert_eq!(r.outcome, crate::checker::multi_review::review::Outcome::Approve);
        let r = recorded.lock().unwrap();
        let user = &r[0].messages[0].content;
        assert!(user.contains("sec-draft"));
        assert!(user.contains("style-draft"));
        assert!(!user.contains("peer review"));
    }

    #[tokio::test]
    async fn synthesize_includes_peer_when_provided() {
        let recorded = Arc::new(Mutex::new(vec![]));
        let client = StubClient {
            responses: Mutex::new(vec![
                r#"{"outcome":"comment","summary":"updated","findings":[]}"#.into(),
            ]),
            recorded: recorded.clone(),
        };
        let drafts = vec![PersonaDraft { persona: "security", raw: "sd".into() }];
        let _ = synthesize(&client, &drafts, "diff", Some("PEER-R1"), 1024).await.unwrap();
        let r = recorded.lock().unwrap();
        assert!(r[0].messages[0].content.contains("PEER-R1"));
    }

    #[test]
    fn render_diff_block_wraps_each_file() {
        let s = render_diff_block(&[file("a.rs", "@@ -1 +1 @@\n+x")]);
        assert!(s.contains("File: a.rs"));
        assert!(s.contains("=== diff begins ==="));
        assert!(s.contains("=== diff ends ==="));
    }
}
