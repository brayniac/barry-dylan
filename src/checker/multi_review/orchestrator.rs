use crate::checker::multi_review::clients::IdentityClients;
use crate::checker::multi_review::identity::Identity;
use crate::checker::multi_review::judge;
use crate::checker::multi_review::persona::Persona;
use crate::checker::multi_review::review::{Outcome, UnifiedReview};
use crate::checker::multi_review::synthesis::{self, PersonaDraft, TokenCount};
use crate::github::pr::ChangedFile;
use crate::telemetry::status::StatusTracker;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum Verdict {
    /// Judge said the two reviewers materially agree. Only Barry posts.
    Agree { barry: UnifiedReview },
    /// Judge said they disagree. Both post.
    Disagree {
        barry: UnifiedReview,
        other_barry: UnifiedReview,
        reason: String,
    },
    /// Other Barry was unavailable; Barry posts alone with success outcome.
    BarryAlone {
        barry: UnifiedReview,
        reason: String,
    },
}

impl Verdict {
    pub fn check_outcome(&self) -> Outcome {
        match self {
            Verdict::Agree { barry } => barry.outcome,
            Verdict::BarryAlone { barry, .. } => barry.outcome,
            Verdict::Disagree { .. } => Outcome::Comment, // neutral check-run
        }
    }
}

pub struct Orchestrator<'a> {
    pub clients: &'a IdentityClients,
    pub personas: &'a [Persona],
    pub tracker: Arc<StatusTracker>,
    pub job_id: i64,
}

impl<'a> Orchestrator<'a> {
    pub async fn run(&self, files: &[ChangedFile]) -> anyhow::Result<Verdict> {
        let span = tracing::info_span!("orchestrator.run", files = files.len());
        let _enter = span.enter();

        tracing::info!("multi-review orchestration starting");
        let diff = synthesis::render_diff_block(files);

        // Phase 1: persona drafts for both identities in parallel.
        // Drafts depend only on (persona, diff) — same in R1 and R2 — so we
        // compute them once and reuse for both synthesis rounds.
        tracing::info!("persona drafts starting (Barry + Other Barry in parallel)");
        let drafts_start = std::time::Instant::now();
        let (barry_drafts, ob_drafts) = tokio::join!(
            self.run_persona_drafts(Identity::Barry, files),
            self.run_persona_drafts(Identity::OtherBarry, files),
        );
        tracing::info!(
            duration_ms = drafts_start.elapsed().as_millis() as u64,
            "persona drafts complete"
        );
        let barry_drafts = match barry_drafts {
            Ok(d) => d,
            Err(e) => return Err(anyhow::anyhow!("barry drafts failed: {e}")),
        };
        let ob_drafts = match ob_drafts {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(?e, "Other Barry persona drafts failed; Barry posts alone");
                let barry_draft_tokens: u64 = barry_drafts.iter().map(|d| d.tokens.input).sum();
                let barry_draft_tokens_out: u64 = barry_drafts.iter().map(|d| d.tokens.output).sum();
                let _ = (barry_draft_tokens, barry_draft_tokens_out);
                // TASK6: add draft tokens to tracker and set R1 synthesis phase here
                let (barry_r1, r1_tokens) = self
                    .synthesize_for(Identity::Barry, &diff, &barry_drafts, None)
                    .await
                    .map_err(|e| anyhow::anyhow!("barry R1 failed: {e}"))?;
                // TASK6: add r1_tokens to tracker here
                let _ = r1_tokens;
                tracing::info!(kind = "barry_alone", "verdict");
                metrics::counter!("barry_multi_review_barry_alone_total").increment(1);
                return Ok(Verdict::BarryAlone {
                    barry: barry_r1,
                    reason: format!("Other Barry unavailable: {e}"),
                });
            }
        };
        // TASK6: add draft token accumulation here (normal path)

        // Phase 2: R1 synthesis (no peer) in parallel.
        tracing::debug!("R1 synthesis starting");
        let r1_start = std::time::Instant::now();
        let (barry_r1_res, ob_r1_res) = tokio::join!(
            self.synthesize_for(Identity::Barry, &diff, &barry_drafts, None),
            self.synthesize_for(Identity::OtherBarry, &diff, &ob_drafts, None),
        );
        let (barry_r1, barry_r1_tokens) = match barry_r1_res {
            Ok(t) => t,
            Err(e) => return Err(anyhow::anyhow!("barry R1 failed: {e}")),
        };
        let (ob_r1, ob_r1_tokens) = match ob_r1_res {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(?e, "Other Barry R1 synthesis failed; Barry posts alone");
                tracing::info!(kind = "barry_alone", "verdict");
                metrics::counter!("barry_multi_review_barry_alone_total").increment(1);
                return Ok(Verdict::BarryAlone {
                    barry: barry_r1,
                    reason: format!("Other Barry unavailable: {e}"),
                });
            }
        };
        tracing::info!(
            duration_ms = r1_start.elapsed().as_millis() as u64,
            barry_outcome = ?barry_r1.outcome,
            ob_outcome = ?ob_r1.outcome,
            "R1 synthesis complete"
        );
        let _ = (barry_r1_tokens, ob_r1_tokens); // wired in Task 6

        // Phase 3: R2 synthesis — each identity reads the other's R1.
        // Reuses the R1 drafts; no fresh persona calls.
        let barry_r1_text = serde_json::to_string(&serde_json::json!({
            "outcome": barry_r1.outcome,
            "summary": barry_r1.summary,
        }))
        .unwrap_or_default();
        let ob_r1_text = serde_json::to_string(&serde_json::json!({
            "outcome": ob_r1.outcome,
            "summary": ob_r1.summary,
        }))
        .unwrap_or_default();
        tracing::debug!("R2 synthesis starting (drafts reused from R1)");
        let r2_start = std::time::Instant::now();
        let (barry_r2_res, ob_r2_res) = tokio::join!(
            self.synthesize_for(Identity::Barry, &diff, &barry_drafts, Some(&ob_r1_text)),
            self.synthesize_for(
                Identity::OtherBarry,
                &diff,
                &ob_drafts,
                Some(&barry_r1_text)
            ),
        );
        let (barry_r2, barry_r2_tokens) = match barry_r2_res {
            Ok(t) => t,
            Err(_) => (barry_r1, TokenCount::default()),
        };
        let (ob_r2, ob_r2_tokens) = match ob_r2_res {
            Ok(t) => t,
            Err(_) => (ob_r1, TokenCount::default()),
        };
        tracing::info!(
            duration_ms = r2_start.elapsed().as_millis() as u64,
            barry_outcome = ?barry_r2.outcome,
            ob_outcome = ?ob_r2.outcome,
            "R2 synthesis complete"
        );
        let _ = (barry_r2_tokens, ob_r2_tokens); // wired in Task 6

        // Judge.
        tracing::debug!("judge starting");
        let judge_start = std::time::Instant::now();
        let verdict = match judge::judge(
            self.clients.judge.as_ref(),
            &barry_r2,
            &ob_r2,
            self.clients.judge_max_tokens.min(512),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(?e, "judge failed; defaulting to disagreement");
                tracing::info!(kind = "disagree", "verdict");
                metrics::counter!("barry_multi_review_judge_total", "verdict" => "disagree")
                    .increment(1);
                return Ok(Verdict::Disagree {
                    barry: barry_r2,
                    other_barry: ob_r2,
                    reason: "judge unavailable".into(),
                });
            }
        };
        tracing::info!(
            duration_ms = judge_start.elapsed().as_millis() as u64,
            agree = verdict.agree,
            reason = %verdict.reason,
            "judge done"
        );

        if verdict.agree {
            tracing::info!(kind = "agree", outcome = ?barry_r2.outcome, "verdict");
            metrics::counter!("barry_multi_review_judge_total", "verdict" => "agree").increment(1);
            Ok(Verdict::Agree { barry: barry_r2 })
        } else {
            tracing::info!(kind = "disagree", "verdict");
            metrics::counter!("barry_multi_review_judge_total", "verdict" => "disagree")
                .increment(1);
            Ok(Verdict::Disagree {
                barry: barry_r2,
                other_barry: ob_r2,
                reason: verdict.reason,
            })
        }
    }

    /// Run every persona in parallel for one identity and collect raw drafts.
    /// Independent of peer review, so a single call's output is reusable in R1 and R2.
    async fn run_persona_drafts(
        &self,
        identity: Identity,
        files: &[ChangedFile],
    ) -> anyhow::Result<Vec<PersonaDraft>> {
        let client = self.clients.for_identity(identity);
        let max_tokens = self.clients.max_tokens_for(identity);
        let span = tracing::info_span!(
            "orchestrator.persona_drafts",
            identity = ?identity,
            personas = self.personas.len(),
            files = files.len()
        );
        let _enter = span.enter();

        tracing::debug!("persona drafts starting");

        let mut futures = Vec::with_capacity(self.personas.len());
        for p in self.personas {
            let c = Arc::clone(client);
            let p = p.clone();
            let diff = self.render_filtered_diff(files, p.name);
            futures.push(
                async move { synthesis::run_persona(c.as_ref(), &p, &diff, max_tokens).await },
            );
        }
        let start = std::time::Instant::now();
        let results = futures::future::join_all(futures).await;
        tracing::info!(
            duration_ms = start.elapsed().as_millis() as u64,
            "persona drafts complete"
        );

        let mut drafts = Vec::with_capacity(results.len());
        for r in results {
            match r {
                Ok(d) => drafts.push(d),
                Err(e) => return Err(anyhow::anyhow!("persona call failed: {e}")),
            }
        }
        Ok(drafts)
    }

    /// Render a diff block containing only files relevant to this persona.
    /// Rust persona only gets .rs files; other personas get all files.
    fn render_filtered_diff(&self, files: &[ChangedFile], persona: &str) -> String {
        let filtered = if persona == "rust" {
            files
                .iter()
                .filter(|f| f.filename.ends_with(".rs"))
                .cloned()
                .collect::<Vec<_>>()
        } else {
            files.to_vec()
        };
        synthesis::render_diff_block(&filtered)
    }

    /// Synthesize a unified review from pre-computed persona drafts. Used for
    /// both R1 (peer=None) and R2 (peer=Some(...)).
    async fn synthesize_for(
        &self,
        identity: Identity,
        diff: &str,
        drafts: &[PersonaDraft],
        peer: Option<&str>,
    ) -> anyhow::Result<(UnifiedReview, TokenCount)> {
        let round = if peer.is_some() { "R2" } else { "R1" };
        let client = self.clients.for_identity(identity);
        let max_tokens = self.clients.max_tokens_for(identity);
        let start = std::time::Instant::now();

        let result = synthesis::synthesize(client.as_ref(), drafts, diff, peer, max_tokens)
            .await
            .map_err(|e| anyhow::anyhow!("synthesis failed: {e}"));

        let duration_ms = start.elapsed().as_millis() as u64;
        match result {
            Ok((review, tokens)) => {
                tracing::info!(
                    identity = ?identity,
                    round,
                    duration_ms,
                    outcome = format!("{:?}", review.outcome),
                    "synthesis done"
                );
                Ok((review, tokens))
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{LlmClient, LlmError, LlmRequest, LlmResponse};
    use crate::telemetry::status::StatusTracker;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    struct ScriptedClient(Arc<Mutex<Vec<Result<String, &'static str>>>>);
    #[async_trait]
    impl LlmClient for ScriptedClient {
        async fn complete(&self, _req: &LlmRequest) -> Result<LlmResponse, LlmError> {
            let next = self.0.lock().unwrap().pop();
            match next {
                Some(Ok(text)) => Ok(LlmResponse {
                    text,
                    input_tokens: None,
                    output_tokens: None,
                }),
                Some(Err(msg)) => Err(LlmError::Shape(msg.into())),
                None => {
                    // Return default response when exhausted
                    Ok(LlmResponse {
                        text: r#"{"outcome":"approve","summary":"LGTM","findings":[]}"#.into(),
                        input_tokens: None,
                        output_tokens: None,
                    })
                }
            }
        }
    }

    fn clients(
        barry: Vec<Result<&'static str, &'static str>>,
        ob: Vec<Result<&'static str, &'static str>>,
        judge: Vec<Result<&'static str, &'static str>>,
    ) -> IdentityClients {
        let to_owned = |v: Vec<Result<&'static str, &'static str>>| {
            Arc::new(Mutex::new(
                v.into_iter().map(|r| r.map(|s| s.to_string())).collect(),
            ))
        };
        IdentityClients {
            barry: Arc::new(ScriptedClient(to_owned(barry))),
            other_barry: Arc::new(ScriptedClient(to_owned(ob))),
            other_other_barry: Arc::new(ScriptedClient(to_owned(vec![]))),
            judge: Arc::new(ScriptedClient(to_owned(judge))),
            barry_max_tokens: 1024,
            other_barry_max_tokens: 1024,
            other_other_barry_max_tokens: 1024,
            judge_max_tokens: 256,
        }
    }

    fn personas() -> Vec<Persona> {
        vec![
            Persona {
                name: "security",
                prompt: Arc::new("you are security".into()),
            },
            Persona {
                name: "rust",
                prompt: Arc::new("you are rust".into()),
            },
        ]
    }

    fn file() -> ChangedFile {
        ChangedFile {
            filename: "a.rs".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 0,
            changes: 1,
            patch: Some("@@ -1 +1 @@\n+x".into()),
        }
    }

    fn approve() -> &'static str {
        r#"{"outcome":"approve","summary":"LGTM","findings":[]}"#
    }
    fn comment() -> &'static str {
        r#"{"outcome":"comment","summary":"check this","findings":[]}"#
    }
    fn agree() -> &'static str {
        r#"{"agree":true,"reason":"same"}"#
    }
    fn disagree() -> &'static str {
        r#"{"agree":false,"reason":"diff"}"#
    }

    #[tokio::test]
    async fn agreement_returns_agree_with_barry() {
        // Per identity: 2 persona calls (security + rust), then synth R1, then synth R2.
        // Responses are consumed in order: security draft, rust draft, synth R1, synth R2.
        let c = clients(
            vec![
                Ok(approve()),
                Ok(approve()), // security + rust drafts for Barry
                Ok(approve()), // synth R1
                Ok(approve()), // synth R2 (reuses drafts)
            ],
            vec![
                Ok(approve()),
                Ok(approve()), // security + rust drafts for OB
                Ok(approve()), // synth R1
                Ok(approve()), // synth R2
            ],
            vec![Ok(agree())],
        );
        let p = personas();
        let v = Orchestrator {
            clients: &c,
            personas: &p,
            tracker: Arc::new(StatusTracker::new()),
            job_id: 0,
        }
        .run(&[file()])
        .await
        .unwrap();
        match v {
            Verdict::Agree { barry } => assert_eq!(barry.outcome, Outcome::Approve),
            other => panic!("wanted Agree, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn disagreement_returns_both() {
        // Barry: security→approve, rust→approve, synth→approve, synth→approve
        // OB: security→comment, rust→comment, synth→comment, synth→comment
        let c = clients(
            vec![
                Ok(approve()),
                Ok(approve()), // security + rust drafts for Barry
                Ok(approve()), // synth R1
                Ok(approve()), // synth R2
            ],
            vec![
                Ok(comment()),
                Ok(comment()), // security + rust drafts for OB
                Ok(comment()), // synth R1
                Ok(comment()), // synth R2
            ],
            vec![Ok(disagree())],
        );
        let p = personas();
        let v = Orchestrator {
            clients: &c,
            personas: &p,
            tracker: Arc::new(StatusTracker::new()),
            job_id: 0,
        }
        .run(&[file()])
        .await
        .unwrap();
        match v {
            Verdict::Disagree {
                barry,
                other_barry,
                reason,
            } => {
                assert_eq!(barry.outcome, Outcome::Approve);
                assert_eq!(other_barry.outcome, Outcome::Comment);
                assert_eq!(reason, "diff");
            }
            other => panic!("wanted Disagree, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ob_failure_yields_barry_alone() {
        let c = clients(
            vec![Ok(approve()), Ok(approve())],
            vec![Err("ob down")],
            vec![],
        );
        let p = personas();
        let v = Orchestrator {
            clients: &c,
            personas: &p,
            tracker: Arc::new(StatusTracker::new()),
            job_id: 0,
        }
        .run(&[file()])
        .await
        .unwrap();
        assert!(matches!(v, Verdict::BarryAlone { .. }));
    }

    #[tokio::test]
    async fn judge_failure_defaults_to_disagree() {
        let c = clients(
            vec![Ok(approve()), Ok(approve()), Ok(approve())],
            vec![Ok(approve()), Ok(approve()), Ok(approve())],
            vec![Err("judge down")],
        );
        let p = personas();
        let v = Orchestrator {
            clients: &c,
            personas: &p,
            tracker: Arc::new(StatusTracker::new()),
            job_id: 0,
        }
        .run(&[file()])
        .await
        .unwrap();
        assert!(matches!(v, Verdict::Disagree { .. }));
    }
}
