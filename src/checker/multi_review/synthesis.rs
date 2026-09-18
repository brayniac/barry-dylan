use crate::checker::multi_review::persona::Persona;
use crate::checker::multi_review::review::{ParseError, UnifiedReview, parse};
use crate::github::pr::ChangedFile;
use crate::llm::{FinishReason, LlmClient, LlmError, LlmMessage, LlmRequest, Role};

const SYNTHESIS_TEMPLATE: &str = include_str!("prompts/synthesis.md");

#[derive(Debug, Clone, Copy, Default)]
pub struct TokenCount {
    pub input: u64,
    pub output: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum SynthesisError {
    #[error("llm: {0}")]
    Llm(#[from] LlmError),
    #[error("parse: {0}")]
    Parse(#[from] ParseError),
    #[error("truncated: model hit max_tokens on both attempts")]
    Truncated,
}

#[derive(Debug)]
pub struct PersonaDraft {
    pub persona: &'static str,
    pub raw: String,
    pub tokens: TokenCount,
}

/// Run a single persona prompt over the diff and return its raw text output.
pub async fn run_persona(
    client: &dyn LlmClient,
    persona: &Persona,
    diff_block: &str,
    max_tokens: u32,
) -> Result<PersonaDraft, LlmError> {
    let req = LlmRequest {
        system: Some(diff_block.to_string()),
        messages: vec![LlmMessage {
            role: Role::User,
            content: format!(
                "{prompt}\n\nReview the diff above. Return ONLY a JSON object with: \
                 {{\"findings\":[{{\"file\":\"<path>\",\"line\":<int>,\"message\":\"<text>\"}}],\
                 \"summary\":\"<one short sentence>\"}}",
                prompt = persona.prompt.as_ref(),
            ),
        }],
        max_tokens,
        temperature: None,
        response_schema: None,
    };
    let resp = client.complete(&req).await?;
    // An empty draft is not a draft. Until 2026-09-18 it was accepted and,
    // when synthesis then failed too, posted as a review of four empty
    // headings; the model had spent its whole budget thinking (infra#15,
    // #17). Failing here makes the check say what happened instead.
    if resp.text.trim().is_empty() {
        return Err(LlmError::Empty {
            finish: resp
                .finish_reason
                .as_ref()
                .map_or_else(|| "unknown".to_string(), ToString::to_string),
            output_tokens: resp.output_tokens.unwrap_or(0),
        });
    }
    Ok(PersonaDraft {
        persona: persona.name,
        raw: resp.text,
        tokens: TokenCount {
            input: u64::from(resp.input_tokens.unwrap_or(0)),
            output: u64::from(resp.output_tokens.unwrap_or(0)),
        },
    })
}

/// Run synthesis over N persona drafts and return a parsed UnifiedReview.
///
/// With `peer`, this is the second round: the reviewer sees the other
/// reviewer's first-round review and revises. Findings that do not overlap
/// are complementary, not a disagreement, and the reviewer is told so; what
/// reaches the judge is then two positions that have each read the other,
/// and "disagree" means contradiction rather than different coverage.
pub async fn synthesize(
    client: &dyn LlmClient,
    drafts: &[PersonaDraft],
    diff_block: &str,
    peer: Option<&UnifiedReview>,
    max_tokens: u32,
) -> Result<(UnifiedReview, TokenCount), SynthesisError> {
    let mut user = String::from(SYNTHESIS_TEMPLATE);
    user.push_str("\n\n=== persona drafts ===\n");
    for d in drafts {
        user.push_str(&format!("--- {} ---\n{}\n", d.persona, d.raw));
    }
    if let Some(peer) = peer {
        user.push_str("\n=== the other reviewer's review of the same diff ===\n");
        user.push_str(&serde_json::to_string_pretty(peer).unwrap_or_default());
        user.push_str(
            "\n=== end of the other review ===\n\
             Treat it as a colleague's first pass over the same code. Adopt the findings \
             you agree with, keeping their file and line. Drop findings of yours that it \
             convincingly rebuts. Keep findings of yours that it simply did not cover: \
             different coverage is complementary, not a disagreement. Contradict it only \
             where you actually disagree about the code or the outcome, and say so in the \
             summary. Do not name the other reviewer.\n",
        );
    }
    let req = LlmRequest {
        system: Some(diff_block.to_string()),
        messages: vec![LlmMessage {
            role: Role::User,
            content: user,
        }],
        max_tokens,
        temperature: None,
        response_schema: Some(review_schema()),
    };
    let mut resp = client.complete(&req).await?;
    if matches!(resp.finish_reason, Some(FinishReason::Length)) {
        tracing::warn!(
            input_tokens = resp.input_tokens,
            output_tokens = resp.output_tokens,
            "synthesis response truncated at max_tokens; retrying once"
        );
        resp = client.complete(&req).await?;
        if matches!(resp.finish_reason, Some(FinishReason::Length)) {
            tracing::warn!("synthesis truncated on retry; giving up");
            return Err(SynthesisError::Truncated);
        }
    }
    let tokens = TokenCount {
        input: u64::from(resp.input_tokens.unwrap_or(0)),
        output: u64::from(resp.output_tokens.unwrap_or(0)),
    };
    Ok((parse(&resp.text)?, tokens))
}

/// What a persona draft is asked to contain. Tolerant on the way in: a draft
/// with only a summary, or only findings, still counts.
#[derive(serde::Deserialize)]
struct Draft {
    #[serde(default)]
    findings: Vec<crate::checker::multi_review::review::UnifiedFinding>,
    #[serde(default)]
    summary: String,
}

/// A review assembled from the persona drafts when synthesis could not run.
///
/// Each draft is parsed as the JSON the persona was asked for and rendered as
/// prose; its findings become the review's findings. Drafts that are empty
/// or not JSON are left out. `None` when nothing was usable: that is not a
/// review, and until 2026-09-18 it was posted anyway as four empty headings
/// (infra#17), or as raw JSON under a heading (infra#15, infra#22).
pub fn review_from_drafts(drafts: &[PersonaDraft]) -> Option<UnifiedReview> {
    let mut lines = Vec::new();
    let mut findings = Vec::new();
    for d in drafts {
        let Some(json) = super::parse_util::locate_json(&d.raw) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<Draft>(json) else {
            continue;
        };
        let summary = parsed.summary.trim();
        if summary.is_empty() && parsed.findings.is_empty() {
            continue;
        }
        lines.push(format!(
            "**{}**: {}",
            d.persona,
            if summary.is_empty() {
                "(findings only)"
            } else {
                summary
            }
        ));
        findings.extend(parsed.findings);
    }
    if lines.is_empty() {
        return None;
    }
    let mut summary = String::from(
        "Synthesis did not complete, so these are the reviewer's own drafts, unreconciled.\n\n",
    );
    summary.push_str(&lines.join("\n"));
    Some(UnifiedReview {
        outcome: crate::checker::multi_review::review::Outcome::Comment,
        summary,
        findings,
    })
}

fn review_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "outcome": {
                "type": "string",
                "enum": ["approve", "comment", "request_changes"]
            },
            "summary": { "type": "string" },
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string" },
                        "line": { "type": "integer" },
                        "message": { "type": "string" }
                    },
                    "required": ["file", "line", "message"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["outcome", "summary", "findings"],
        "additionalProperties": false
    })
}

/// What the model cannot know from its training: today's date and the
/// toolchain in use.
///
/// A local model's training ends months before it reviews anything, so a
/// correct date in a comment reads to it as the future and the current Rust
/// edition as a typo. Qwen3.6-35B on 2026-09-18 filed four findings against
/// comments naming that day, and Qwen3.5-9B blocked a PR over
/// `edition = "2024"`. Stating both up front costs a line.
pub fn review_context() -> String {
    let today = time::OffsetDateTime::now_utc().date();
    format!(
        "=== context ===\n\
         Today is {today} (UTC). The current Rust toolchain is {rustc} and the current \
         Rust edition is {edition}; both exist. Dates up to today are not in the future. \
         Your training may predate these facts; trust them over it.\n\
         === context ends ===\n",
        rustc = env!("BARRY_RUSTC_VERSION"),
        edition = env!("BARRY_RUST_EDITION"),
    )
}

/// Render a diff block from changed files, suitable for embedding in a user message.
pub fn render_diff_block(files: &[ChangedFile]) -> String {
    let mut s = review_context();
    s.push_str("=== diff begins ===\n");
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
    use crate::llm::{FinishReason, LlmResponse};
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
                finish_reason: None,
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
    async fn run_persona_puts_diff_in_system_and_prompt_in_user() {
        let recorded = Arc::new(Mutex::new(vec![]));
        let client = StubClient {
            responses: Mutex::new(vec![r#"{"findings":[],"summary":"ok"}"#.into()]),
            recorded: recorded.clone(),
        };
        let p = persona("security");
        let d = run_persona(&client, &p, "DIFF-BLOCK", 1024).await.unwrap();
        assert_eq!(d.persona, "security");
        let r = recorded.lock().unwrap();
        assert_eq!(r[0].system.as_deref(), Some("DIFF-BLOCK"));
        assert!(r[0].messages[0].content.contains("you are security"));
    }

    #[tokio::test]
    async fn a_persona_that_returns_nothing_is_an_error_not_a_draft() {
        let client = ScriptedClient(Mutex::new(vec![LlmResponse {
            text: "   ".into(),
            input_tokens: Some(7000),
            output_tokens: Some(16384),
            finish_reason: Some(FinishReason::Length),
        }]));
        let err = run_persona(&client, &persona("security"), "DIFF", 16384)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                LlmError::Empty {
                    output_tokens: 16384,
                    ..
                }
            ),
            "{err}"
        );
        assert!(err.to_string().contains("no content"), "{err}");
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
            PersonaDraft {
                persona: "security",
                raw: "sec-draft".into(),
                tokens: TokenCount::default(),
            },
            PersonaDraft {
                persona: "style",
                raw: "style-draft".into(),
                tokens: TokenCount::default(),
            },
        ];
        let (r, _) = synthesize(&client, &drafts, "DIFF-BLOCK", None, 1024)
            .await
            .unwrap();
        assert_eq!(
            r.outcome,
            crate::checker::multi_review::review::Outcome::Approve
        );
        let r = recorded.lock().unwrap();
        assert_eq!(r[0].system.as_deref(), Some("DIFF-BLOCK"));
        let user = &r[0].messages[0].content;
        assert!(user.contains("sec-draft"));
        assert!(user.contains("style-draft"));
        assert!(!user.contains("peer review"));
    }

    #[test]
    fn the_diff_block_opens_with_the_date_and_the_toolchain() {
        let s = render_diff_block(&[file("a.rs", "@@ -1 +1 @@\n+x")]);
        assert!(s.starts_with("=== context ===\nToday is 20"), "{s}");
        assert!(s.contains("rustc 1."), "{s}");
        assert!(s.contains("Rust edition is 2024"), "{s}");
        assert!(
            s.contains("=== context ends ===\n=== diff begins ==="),
            "{s}"
        );
    }

    #[test]
    fn render_diff_block_wraps_each_file() {
        let s = render_diff_block(&[file("a.rs", "@@ -1 +1 @@\n+x")]);
        assert!(s.contains("File: a.rs"));
        assert!(s.contains("=== diff begins ==="));
        assert!(s.contains("=== diff ends ==="));
    }

    struct ScriptedClient(Mutex<Vec<LlmResponse>>);

    #[async_trait]
    impl LlmClient for ScriptedClient {
        async fn complete(&self, _req: &LlmRequest) -> Result<LlmResponse, LlmError> {
            let resp = self.0.lock().unwrap().remove(0);
            Ok(resp)
        }
    }

    fn truncated() -> LlmResponse {
        LlmResponse {
            text: "no json here, just gibberish".into(),
            input_tokens: Some(10),
            output_tokens: Some(1024),
            finish_reason: Some(FinishReason::Length),
        }
    }

    fn ok_review() -> LlmResponse {
        LlmResponse {
            text: r#"{"outcome":"approve","summary":"LGTM","findings":[]}"#.into(),
            input_tokens: Some(10),
            output_tokens: Some(50),
            finish_reason: Some(FinishReason::Stop),
        }
    }

    #[tokio::test]
    async fn synthesize_retries_once_on_length_then_succeeds() {
        let client = ScriptedClient(Mutex::new(vec![truncated(), ok_review()]));
        let (review, _) = synthesize(&client, &[], "diff", None, 1024).await.unwrap();
        assert_eq!(
            review.outcome,
            crate::checker::multi_review::review::Outcome::Approve
        );
    }

    #[tokio::test]
    async fn synthesize_returns_truncated_after_both_attempts_fail() {
        let client = ScriptedClient(Mutex::new(vec![truncated(), truncated()]));
        let err = synthesize(&client, &[], "diff", None, 1024)
            .await
            .unwrap_err();
        assert!(matches!(err, SynthesisError::Truncated));
    }

    #[tokio::test]
    async fn synthesize_does_not_retry_on_stop() {
        let client = ScriptedClient(Mutex::new(vec![ok_review()]));
        let (review, _) = synthesize(&client, &[], "diff", None, 1024).await.unwrap();
        assert_eq!(
            review.outcome,
            crate::checker::multi_review::review::Outcome::Approve
        );
    }

    #[tokio::test]
    async fn the_peer_round_shows_the_other_review_and_how_to_treat_it() {
        let recorded = Arc::new(Mutex::new(vec![]));
        let client = StubClient {
            responses: Mutex::new(vec![
                r#"{"outcome":"comment","summary":"revised","findings":[]}"#.into(),
            ]),
            recorded: recorded.clone(),
        };
        let peer = crate::checker::multi_review::review::parse(
            r#"{"outcome":"approve","summary":"PEER-SAID-THIS","findings":[{"file":"b.rs","line":9,"message":"peer finding"}]}"#,
        )
        .unwrap();
        let drafts = vec![PersonaDraft {
            persona: "security",
            raw: "sd".into(),
            tokens: TokenCount::default(),
        }];
        let (r, _) = synthesize(&client, &drafts, "diff", Some(&peer), 1024)
            .await
            .unwrap();
        assert_eq!(r.summary, "revised");
        let user = &recorded.lock().unwrap()[0].messages[0].content;
        assert!(user.contains("PEER-SAID-THIS"), "{user}");
        assert!(user.contains("peer finding"), "{user}");
        assert!(user.contains("complementary, not a disagreement"), "{user}");
    }

    #[test]
    fn drafts_are_rendered_as_prose_with_their_findings() {
        let drafts = vec![
            PersonaDraft {
                persona: "security",
                raw: r#"{"findings":[],"summary":"No security issues."}"#.into(),
                tokens: TokenCount::default(),
            },
            PersonaDraft {
                persona: "correctness",
                raw: "".into(),
                tokens: TokenCount::default(),
            },
            PersonaDraft {
                persona: "rust",
                raw: "```json\n{\"findings\":[{\"file\":\"a.rs\",\"line\":4,\"message\":\"unwrap\"}],\"summary\":\"One unwrap.\"}\n```".into(),
                tokens: TokenCount::default(),
            },
        ];
        let r = review_from_drafts(&drafts).expect("two usable drafts");
        assert_eq!(
            r.outcome,
            crate::checker::multi_review::review::Outcome::Comment
        );
        assert!(
            r.summary.contains("**security**: No security issues."),
            "{}",
            r.summary
        );
        assert!(r.summary.contains("**rust**: One unwrap."), "{}", r.summary);
        assert!(!r.summary.contains("correctness"), "{}", r.summary);
        assert!(!r.summary.contains("{\""), "raw JSON leaked: {}", r.summary);
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].line, 4);
    }

    #[test]
    fn no_usable_draft_is_no_review() {
        let drafts = vec![
            PersonaDraft {
                persona: "security",
                raw: "   ".into(),
                tokens: TokenCount::default(),
            },
            PersonaDraft {
                persona: "rust",
                raw: "not json at all".into(),
                tokens: TokenCount::default(),
            },
        ];
        assert!(review_from_drafts(&drafts).is_none());
    }
}
