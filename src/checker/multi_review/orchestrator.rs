use crate::checker::multi_review::clients::IdentityClients;
use crate::checker::multi_review::identity::Identity;
use crate::checker::multi_review::judge;
use crate::checker::multi_review::persona::Persona;
use crate::checker::multi_review::review::{Outcome, UnifiedReview};
use crate::checker::multi_review::synthesis::{self, PersonaDraft, SynthesisError, TokenCount};
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
        self.tracker.set_phase(self.job_id, "persona drafts");
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
                let barry_draft_tokens_out: u64 =
                    barry_drafts.iter().map(|d| d.tokens.output).sum();
                self.tracker
                    .add_tokens(self.job_id, barry_draft_tokens, barry_draft_tokens_out);
                self.tracker.set_phase(self.job_id, "R1 synthesis");
                let (barry_r1, r1_tokens) = match self
                    .synthesize_for(Identity::Barry, &diff, &barry_drafts)
                    .await
                {
                    Ok(t) => t,
                    Err(SynthesisError::Truncated) => {
                        tracing::warn!(
                            "barry R1 synthesis truncated; using persona-draft fallback"
                        );
                        metrics::counter!("barry_multi_review_truncated_total", "phase" => "r1_synthesis").increment(1);
                        return Ok(Verdict::BarryAlone {
                            barry: synthesis::review_from_drafts(&barry_drafts),
                            reason: "synthesis truncated".into(),
                        });
                    }
                    Err(e) => return Err(anyhow::anyhow!("barry R1 failed: {e}")),
                };
                self.tracker
                    .add_tokens(self.job_id, r1_tokens.input, r1_tokens.output);
                tracing::info!(kind = "barry_alone", "verdict");
                metrics::counter!("barry_multi_review_barry_alone_total").increment(1);
                return Ok(Verdict::BarryAlone {
                    barry: barry_r1,
                    reason: format!("Other Barry unavailable: {e}"),
                });
            }
        };
        let draft_tok_in: u64 = barry_drafts
            .iter()
            .chain(ob_drafts.iter())
            .map(|d| d.tokens.input)
            .sum();
        let draft_tok_out: u64 = barry_drafts
            .iter()
            .chain(ob_drafts.iter())
            .map(|d| d.tokens.output)
            .sum();
        self.tracker
            .add_tokens(self.job_id, draft_tok_in, draft_tok_out);

        // Phase 2: R1 synthesis (no peer) in parallel.
        self.tracker.set_phase(self.job_id, "R1 synthesis");
        tracing::debug!("R1 synthesis starting");
        let r1_start = std::time::Instant::now();
        let (barry_r1_res, ob_r1_res) = tokio::join!(
            self.synthesize_for(Identity::Barry, &diff, &barry_drafts),
            self.synthesize_for(Identity::OtherBarry, &diff, &ob_drafts),
        );
        let (barry_r1, barry_r1_tokens) = match barry_r1_res {
            Ok(t) => t,
            Err(SynthesisError::Truncated) => {
                tracing::warn!("barry R1 synthesis truncated; using persona-draft fallback");
                metrics::counter!("barry_multi_review_truncated_total", "phase" => "r1_synthesis")
                    .increment(1);
                return Ok(Verdict::BarryAlone {
                    barry: synthesis::review_from_drafts(&barry_drafts),
                    reason: "synthesis truncated".into(),
                });
            }
            Err(e) => return Err(anyhow::anyhow!("barry R1 failed: {e}")),
        };
        let (ob_r1, ob_r1_tokens) = match ob_r1_res {
            Ok(t) => t,
            Err(e) => {
                self.tracker
                    .add_tokens(self.job_id, barry_r1_tokens.input, barry_r1_tokens.output);
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
        self.tracker.add_tokens(
            self.job_id,
            barry_r1_tokens.input + ob_r1_tokens.input,
            barry_r1_tokens.output + ob_r1_tokens.output,
        );

        Ok(self.judge_reviews(barry_r1, ob_r1).await)
    }

    /// Reconcile two finished reviews into a verdict.
    ///
    /// Separate from [`Self::run`] because where the reviews came from does not
    /// change how they are reconciled: the same judge runs over reviews
    /// produced locally and over reviews produced by a rack job, which only
    /// ever returns finished reviews and never the drafts behind them.
    ///
    /// A judge that fails is not fatal. Barry posts alone rather than the whole
    /// review being lost to a reconciliation step.
    pub async fn judge_reviews(&self, barry: UnifiedReview, other: UnifiedReview) -> Verdict {
        self.tracker.set_phase(self.job_id, "judge");
        tracing::debug!("judge starting");
        let judge_start = std::time::Instant::now();
        let verdict = match judge::judge(
            self.clients.judge.as_ref(),
            &barry,
            &other,
            self.clients.judge_max_tokens.min(512),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(?e, "judge failed; posting Barry alone");
                tracing::info!(kind = "barry_alone", "verdict");
                metrics::counter!("barry_multi_review_barry_alone_total").increment(1);
                return Verdict::BarryAlone {
                    barry,
                    reason: "judge unavailable".into(),
                };
            }
        };
        tracing::info!(
            duration_ms = judge_start.elapsed().as_millis() as u64,
            agree = verdict.agree,
            reason = %verdict.reason,
            "judge done"
        );
        self.tracker
            .add_tokens(self.job_id, verdict.tokens.input, verdict.tokens.output);

        if verdict.agree {
            tracing::info!(kind = "agree", outcome = ?barry.outcome, "verdict");
            metrics::counter!("barry_multi_review_judge_total", "verdict" => "agree").increment(1);
            Verdict::Agree { barry }
        } else {
            tracing::info!(kind = "disagree", "verdict");
            metrics::counter!("barry_multi_review_judge_total", "verdict" => "disagree")
                .increment(1);
            Verdict::Disagree {
                barry,
                other_barry: other,
                reason: verdict.reason,
            }
        }
    }

    /// Run Barry's pipeline alone (drafts + R1 synthesis). Used when OB is
    /// known to be unavailable before any LLM calls (e.g., not installed).
    /// No OB or judge calls are made.
    pub async fn run_barry_only(
        &self,
        files: &[ChangedFile],
        reason: String,
    ) -> anyhow::Result<Verdict> {
        let span = tracing::info_span!("orchestrator.run_barry_only", files = files.len());
        let _enter = span.enter();

        self.tracker.set_phase(self.job_id, "persona drafts");
        let diff = synthesis::render_diff_block(files);
        tracing::debug!("Barry-only drafts starting");
        let barry_drafts = self
            .run_persona_drafts(Identity::Barry, files)
            .await
            .map_err(|e| anyhow::anyhow!("barry drafts failed: {e}"))?;
        let draft_tok_in: u64 = barry_drafts.iter().map(|d| d.tokens.input).sum();
        let draft_tok_out: u64 = barry_drafts.iter().map(|d| d.tokens.output).sum();
        self.tracker
            .add_tokens(self.job_id, draft_tok_in, draft_tok_out);
        self.tracker.set_phase(self.job_id, "R1 synthesis");
        let (barry_r1, r1_tokens) = match self
            .synthesize_for(Identity::Barry, &diff, &barry_drafts)
            .await
        {
            Ok(t) => t,
            Err(SynthesisError::Truncated) => {
                tracing::warn!(
                    "barry synthesis truncated in run_barry_only; using persona-draft fallback"
                );
                metrics::counter!("barry_multi_review_truncated_total", "phase" => "r1_synthesis")
                    .increment(1);
                return Ok(Verdict::BarryAlone {
                    barry: synthesis::review_from_drafts(&barry_drafts),
                    reason: "synthesis truncated".into(),
                });
            }
            Err(e) => return Err(anyhow::anyhow!("barry synthesis failed: {e}")),
        };
        self.tracker
            .add_tokens(self.job_id, r1_tokens.input, r1_tokens.output);
        tracing::info!(kind = "barry_alone", "verdict");
        metrics::counter!("barry_multi_review_barry_alone_total").increment(1);
        Ok(Verdict::BarryAlone {
            barry: barry_r1,
            reason,
        })
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

    /// Synthesize a unified review from pre-computed persona drafts.
    async fn synthesize_for(
        &self,
        identity: Identity,
        diff: &str,
        drafts: &[PersonaDraft],
    ) -> Result<(UnifiedReview, TokenCount), synthesis::SynthesisError> {
        let client = self.clients.for_identity(identity);
        let max_tokens = self.clients.max_tokens_for(identity);
        let start = std::time::Instant::now();

        let result = synthesis::synthesize(client.as_ref(), drafts, diff, max_tokens).await;

        let duration_ms = start.elapsed().as_millis() as u64;
        match result {
            Ok((review, tokens)) => {
                tracing::info!(
                    identity = ?identity,
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

    struct ScriptedClient(Arc<Mutex<Vec<Result<LlmResponse, &'static str>>>>);

    #[async_trait]
    impl LlmClient for ScriptedClient {
        async fn complete(&self, _req: &LlmRequest) -> Result<LlmResponse, LlmError> {
            let next = self.0.lock().unwrap().pop();
            match next {
                Some(Ok(resp)) => Ok(resp),
                Some(Err(msg)) => Err(LlmError::Shape(msg.into())),
                None => Ok(LlmResponse {
                    text: r#"{"outcome":"approve","summary":"LGTM","findings":[]}"#.into(),
                    input_tokens: None,
                    output_tokens: None,
                    finish_reason: None,
                }),
            }
        }
    }

    fn ok_resp(text: &'static str) -> LlmResponse {
        LlmResponse {
            text: text.into(),
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
        }
    }

    fn truncated_resp() -> LlmResponse {
        LlmResponse {
            text: "gibberish no json".into(),
            input_tokens: None,
            output_tokens: None,
            finish_reason: Some(crate::llm::FinishReason::Length),
        }
    }

    fn approve() -> LlmResponse {
        ok_resp(r#"{"outcome":"approve","summary":"LGTM","findings":[]}"#)
    }
    fn comment() -> LlmResponse {
        ok_resp(r#"{"outcome":"comment","summary":"check this","findings":[]}"#)
    }
    fn agree() -> LlmResponse {
        ok_resp(r#"{"agree":true,"reason":"same"}"#)
    }
    fn disagree() -> LlmResponse {
        ok_resp(r#"{"agree":false,"reason":"diff"}"#)
    }

    fn clients(
        barry: Vec<Result<LlmResponse, &'static str>>,
        ob: Vec<Result<LlmResponse, &'static str>>,
        judge: Vec<Result<LlmResponse, &'static str>>,
    ) -> IdentityClients {
        let to_owned = |v: Vec<Result<LlmResponse, &'static str>>| Arc::new(Mutex::new(v));
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

    #[tokio::test]
    async fn agreement_returns_agree_with_barry() {
        // Per identity: 2 persona calls (security + rust) then synthesis.
        let c = clients(
            vec![Ok(approve()), Ok(approve()), Ok(approve())],
            vec![Ok(approve()), Ok(approve()), Ok(approve())],
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
        let c = clients(
            vec![Ok(approve()), Ok(approve()), Ok(approve())],
            vec![Ok(comment()), Ok(comment()), Ok(comment())],
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
        // Barry: 2 persona drafts + 1 R1 synthesis (OB draft fails so Barry synths alone)
        let c = clients(
            vec![Ok(approve()), Ok(approve()), Ok(approve())],
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
    async fn judge_failure_returns_barry_alone() {
        // Both reviewers run full pipeline; judge errors → orchestrator
        // should fall back to BarryAlone, not Disagree.
        let c = clients(
            vec![Ok(approve()), Ok(approve()), Ok(approve())],
            vec![Ok(comment()), Ok(comment()), Ok(comment())],
            vec![Err("transport boom"), Err("transport boom")],
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
            Verdict::BarryAlone { reason, .. } => {
                assert_eq!(reason, "judge unavailable");
            }
            other => panic!("wanted BarryAlone, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_barry_only_skips_ob_and_judge() {
        // Barry's drafts (security + rust) + R1 synth = 3 calls.
        // OB and judge clients must not be called.
        let c = clients(
            vec![Ok(approve()), Ok(approve()), Ok(approve())],
            vec![], // OB client must not be called.
            vec![], // Judge client must not be called.
        );
        let p = personas();
        let v = Orchestrator {
            clients: &c,
            personas: &p,
            tracker: Arc::new(StatusTracker::new()),
            job_id: 0,
        }
        .run_barry_only(&[file()], "Other Barry not installed".into())
        .await
        .unwrap();
        match v {
            Verdict::BarryAlone { barry, reason } => {
                assert_eq!(barry.outcome, Outcome::Approve);
                assert_eq!(reason, "Other Barry not installed");
            }
            other => panic!("wanted BarryAlone, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn barry_r1_truncation_returns_barry_alone_with_draft_content() {
        // Barry: 2 persona drafts OK, then R1 synth fires twice (retry) and both truncate.
        // OB: 2 persona drafts (run in parallel, wasted). Judge must not be called.
        // Note: ScriptedClient uses pop() (LIFO), so last element is consumed first.
        let c = clients(
            vec![
                Ok(truncated_resp()), // R1 synth retry (consumed second by synthesize)
                Ok(truncated_resp()), // R1 synth first attempt (consumed first by synthesize)
                Ok(approve()),        // rust draft
                Ok(approve()),        // security draft
            ],
            vec![Ok(approve()), Ok(approve())],
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
        match v {
            Verdict::BarryAlone { barry, reason } => {
                assert_eq!(reason, "synthesis truncated");
                assert_eq!(barry.outcome, Outcome::Comment);
            }
            other => panic!("wanted BarryAlone, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_barry_only_truncation_returns_barry_alone_with_draft_content() {
        // Barry: 2 persona drafts OK, then R1 synth fires twice and both truncate.
        let c = clients(
            vec![
                Ok(truncated_resp()), // R1 synth retry
                Ok(truncated_resp()), // R1 synth first attempt
                Ok(approve()),        // rust draft
                Ok(approve()),        // security draft
            ],
            vec![],
            vec![],
        );
        let p = personas();
        let v = Orchestrator {
            clients: &c,
            personas: &p,
            tracker: Arc::new(StatusTracker::new()),
            job_id: 0,
        }
        .run_barry_only(&[file()], "OB not installed".into())
        .await
        .unwrap();
        match v {
            Verdict::BarryAlone { barry, reason } => {
                assert_eq!(reason, "synthesis truncated");
                assert_eq!(barry.outcome, Outcome::Comment);
            }
            other => panic!("wanted BarryAlone, got {other:?}"),
        }
    }
}
