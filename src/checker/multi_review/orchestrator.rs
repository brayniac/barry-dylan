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

/// Rough token count for a prompt of `bytes` bytes. Diffs tokenize densely,
/// so three bytes per token is on the pessimistic side, which is the side to
/// be on when the cost of guessing low is a failed review.
fn estimate_tokens(bytes: usize) -> u32 {
    (bytes / 3) as u32
}

/// How many persona requests may be in flight at once.
///
/// A local llama-server holds one context window shared by every request in
/// flight (unified KV, its default when the slot count is auto), and a
/// request that runs the shared window out fails with "Context size has been
/// exceeded". Each persona costs its prompt plus up to `max_tokens` of
/// output, so at most `context / cost` of them fit together. barry-dylan#38
/// on 2026-09-18 is the case: a 466-line diff, four personas, 16384 tokens
/// each, a 65536 window -- the 27B got through, the 9B thought longer and
/// did not.
///
/// Never below one: a single persona that does not fit is the model's problem
/// to report, not a reason to run nothing. Never above the persona count.
/// Unknown context means a hosted API, which has no shared window to run out.
pub fn persona_concurrency(
    context_size: Option<u32>,
    max_tokens: u32,
    largest_prompt_bytes: usize,
    personas: usize,
) -> usize {
    let Some(context) = context_size else {
        return personas.max(1);
    };
    let cost = u64::from(max_tokens) + u64::from(estimate_tokens(largest_prompt_bytes));
    let fit = (u64::from(context) / cost.max(1)) as usize;
    fit.clamp(1, personas.max(1))
}

/// The metric label for a synthesis fallback. Barry's own review of #48
/// pointed out that counting parse failures under `truncated_total` misnamed
/// them; the counter is now named for what it counts and labelled by why.
fn synthesis_fallback_metric(e: &SynthesisError) -> &'static str {
    match e {
        SynthesisError::Truncated => "truncated",
        SynthesisError::Parse(_) => "invalid_json",
        _ => "other",
    }
}

/// What to tell the reader when synthesis gave nothing usable.
fn synthesis_fallback_reason(e: &SynthesisError) -> &'static str {
    match e {
        SynthesisError::Truncated => "synthesis truncated",
        SynthesisError::Parse(_) => "synthesis produced invalid JSON twice",
        _ => "synthesis failed",
    }
}

/// Barry alone, from the persona drafts, when synthesis could not run.
///
/// An error rather than a review when no draft was usable: the check-run
/// then says what happened instead of four empty headings.
fn barry_from_drafts(drafts: &[PersonaDraft], reason: &str) -> anyhow::Result<Verdict> {
    match synthesis::review_from_drafts(drafts) {
        Some(barry) => Ok(Verdict::BarryAlone {
            barry,
            reason: reason.into(),
        }),
        None => Err(anyhow::anyhow!(
            "{reason}, and no persona draft was usable to review from"
        )),
    }
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
                    .synthesize_for(Identity::Barry, &diff, &barry_drafts, None)
                    .await
                {
                    Ok(t) => t,
                    Err(e @ (SynthesisError::Truncated | SynthesisError::Parse(_))) => {
                        let reason = synthesis_fallback_reason(&e);
                        tracing::warn!(%e, "barry synthesis could not produce a review; using the persona drafts");
                        metrics::counter!("barry_multi_review_synthesis_fallback_total", "reason" => synthesis_fallback_metric(&e)).increment(1);
                        return barry_from_drafts(&barry_drafts, reason);
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
            self.synthesize_for(Identity::Barry, &diff, &barry_drafts, None),
            self.synthesize_for(Identity::OtherBarry, &diff, &ob_drafts, None),
        );
        let (barry_r1, barry_r1_tokens) = match barry_r1_res {
            Ok(t) => t,
            Err(e @ (SynthesisError::Truncated | SynthesisError::Parse(_))) => {
                let reason = synthesis_fallback_reason(&e);
                tracing::warn!(%e, "barry synthesis could not produce a review; using the persona drafts");
                metrics::counter!("barry_multi_review_synthesis_fallback_total", "reason" => synthesis_fallback_metric(&e)).increment(1);
                return barry_from_drafts(&barry_drafts, reason);
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

        // Phase 3: the peer round. Each reviewer reads the other's first
        // review and revises, in parallel. Removed on 2026-05-21 (#34) when a
        // synthesis call was a round trip to a hosted model; restored on
        // 2026-09-18 when it is ~45 s against a local one, because without it
        // the judge was posting two reviews that raised the same concern in
        // different words as a "disagreement", and reviews with disjoint
        // findings as one too. A reviewer whose second round fails keeps its
        // first: one revised review is still better than none.
        self.tracker.set_phase(self.job_id, "peer round");
        let r2_start = std::time::Instant::now();
        let (barry_r2_res, ob_r2_res) = tokio::join!(
            self.synthesize_for(Identity::Barry, &diff, &barry_drafts, Some(&ob_r1)),
            self.synthesize_for(Identity::OtherBarry, &diff, &ob_drafts, Some(&barry_r1)),
        );
        let mut tokens_in = 0;
        let mut tokens_out = 0;
        let mut settle = |identity: &str,
                          res: Result<(UnifiedReview, TokenCount), SynthesisError>,
                          r1: UnifiedReview| match res {
            Ok((r2, t)) => {
                tokens_in += t.input;
                tokens_out += t.output;
                metrics::counter!("barry_multi_review_peer_round_total", "outcome" => "revised")
                    .increment(1);
                r2
            }
            Err(e) => {
                tracing::warn!(identity, ?e, "peer round failed; keeping the first review");
                metrics::counter!("barry_multi_review_peer_round_total", "outcome" => "kept_first")
                    .increment(1);
                r1
            }
        };
        let barry = settle("barry", barry_r2_res, barry_r1);
        let other = settle("other_barry", ob_r2_res, ob_r1);
        self.tracker.add_tokens(self.job_id, tokens_in, tokens_out);
        tracing::info!(
            duration_ms = r2_start.elapsed().as_millis() as u64,
            barry_outcome = ?barry.outcome,
            ob_outcome = ?other.outcome,
            "peer round complete"
        );

        Ok(self.judge_reviews(barry, other).await)
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
            // The configured budget, not `.min(512)`. The clamp was sized for
            // a small hosted model that answers in a sentence, and it silently
            // undid every larger budget anyone configured: a thinking model
            // spends 512 tokens reasoning and returns nothing, which posts as
            // "judge unavailable; Barry alone" and drops the second review.
            self.clients.judge_max_tokens,
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

        self.verdict_from(barry, other, verdict.agree, verdict.reason)
    }

    /// Turn a decision into a [`Verdict`], wherever the decision was made.
    ///
    /// Shared by the judge that runs here and the one that runs in a rack
    /// guest, so that "what agreement means for what gets posted" is decided
    /// once. The counters are the same too: a verdict reached on the rack is
    /// still a verdict, and splitting the metric by where it was decided would
    /// make the agree/disagree ratio unreadable across a config change.
    pub fn verdict_from(
        &self,
        barry: UnifiedReview,
        other: UnifiedReview,
        agree: bool,
        reason: String,
    ) -> Verdict {
        if agree {
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
                reason,
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
            .synthesize_for(Identity::Barry, &diff, &barry_drafts, None)
            .await
        {
            Ok(t) => t,
            Err(e @ (SynthesisError::Truncated | SynthesisError::Parse(_))) => {
                let reason = synthesis_fallback_reason(&e);
                tracing::warn!(%e, "barry synthesis could not produce a review; using the persona drafts");
                metrics::counter!("barry_multi_review_synthesis_fallback_total", "reason" => synthesis_fallback_metric(&e)).increment(1);
                return barry_from_drafts(&barry_drafts, reason);
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

        let mut prepared = Vec::with_capacity(self.personas.len());
        for p in self.personas {
            let diff = self.render_filtered_diff(files, p.name);
            prepared.push((p.clone(), diff));
        }
        let largest = prepared
            .iter()
            .map(|(p, diff)| p.prompt.len() + diff.len())
            .max()
            .unwrap_or(0);
        let context_size = self.clients.context_size_for(identity);
        let at_once = persona_concurrency(context_size, max_tokens, largest, self.personas.len());
        if at_once < self.personas.len() {
            tracing::info!(
                at_once,
                personas = self.personas.len(),
                context_size,
                max_tokens,
                largest_prompt_bytes = largest,
                "personas run in turns so that those in flight fit the context together"
            );
        }
        let gate = Arc::new(tokio::sync::Semaphore::new(at_once));

        let mut futures = Vec::with_capacity(self.personas.len());
        for (p, diff) in prepared {
            let c = Arc::clone(client);
            let gate = Arc::clone(&gate);
            futures.push(async move {
                let _slot = gate.acquire().await.expect("semaphore is never closed");
                synthesis::run_persona(c.as_ref(), &p, &diff, max_tokens).await
            });
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
        peer: Option<&UnifiedReview>,
    ) -> Result<(UnifiedReview, TokenCount), synthesis::SynthesisError> {
        let client = self.clients.for_identity(identity);
        let max_tokens = self.clients.max_tokens_for(identity);
        let start = std::time::Instant::now();

        let result = synthesis::synthesize(client.as_ref(), drafts, diff, peer, max_tokens).await;

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

    /// Records the max_tokens of every request it sees.
    struct BudgetClient(Arc<Mutex<Vec<u32>>>);

    #[async_trait]
    impl LlmClient for BudgetClient {
        async fn complete(&self, req: &LlmRequest) -> Result<LlmResponse, LlmError> {
            self.0.lock().unwrap().push(req.max_tokens);
            Ok(LlmResponse {
                text: r#"{"agree":true,"reason":"same"}"#.into(),
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn the_judge_gets_its_configured_budget_not_a_512_clamp() {
        let seen = Arc::new(Mutex::new(vec![]));
        let judge: Arc<dyn LlmClient> = Arc::new(BudgetClient(seen.clone()));
        let clients = IdentityClients {
            barry: judge.clone(),
            other_barry: judge.clone(),
            other_other_barry: judge.clone(),
            judge,
            barry_max_tokens: 1,
            other_barry_max_tokens: 1,
            other_other_barry_max_tokens: 1,
            judge_max_tokens: 4096,
            barry_context_size: None,
            other_barry_context_size: None,
            other_other_barry_context_size: None,
        };
        let o = Orchestrator {
            clients: &clients,
            personas: &[],
            tracker: Arc::new(StatusTracker::new()),
            job_id: 1,
        };
        let review = crate::checker::multi_review::review::parse(
            r#"{"outcome":"approve","summary":"x","findings":[]}"#,
        )
        .unwrap();
        let _ = o.judge_reviews(review.clone(), review).await;
        assert_eq!(seen.lock().unwrap().as_slice(), &[4096]);
    }

    #[test]
    fn unknown_context_runs_every_persona_at_once() {
        assert_eq!(persona_concurrency(None, 16384, 1_000_000, 4), 4);
    }

    #[test]
    fn personas_in_flight_together_must_fit_the_window() {
        // barry-dylan#38: ~21 KB of diff, 16384 max_tokens, 65536 window.
        // 16384 + 7000 ~= 23.4k each, so two fit and four do not.
        assert_eq!(persona_concurrency(Some(65536), 16384, 21_000, 4), 2);
        // A small diff fits three, as #37 did.
        assert_eq!(persona_concurrency(Some(65536), 16384, 4_700, 4), 3);
        // The old 4096 budget fit all four.
        assert_eq!(persona_concurrency(Some(65536), 4096, 21_000, 4), 4);
    }

    #[test]
    fn a_persona_that_does_not_fit_alone_still_runs_alone() {
        // Not our error to pre-empt: the server reports it, and the log says
        // what to change.
        assert_eq!(persona_concurrency(Some(8192), 16384, 30_000, 4), 1);
    }

    /// A client that records how many requests it has in flight at once.
    struct CountingClient {
        in_flight: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl LlmClient for CountingClient {
        async fn complete(&self, _req: &LlmRequest) -> Result<LlmResponse, LlmError> {
            use std::sync::atomic::Ordering::SeqCst;
            let now = self.in_flight.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(now, SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            self.in_flight.fetch_sub(1, SeqCst);
            Ok(LlmResponse {
                text: r#"{"findings":[],"summary":"ok"}"#.into(),
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn the_gate_holds_personas_back_when_the_window_is_small() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let peak = Arc::new(AtomicUsize::new(0));
        let client: Arc<dyn LlmClient> = Arc::new(CountingClient {
            in_flight: Arc::new(AtomicUsize::new(0)),
            peak: peak.clone(),
        });
        // Four personas at ~1000 tokens each into a 2500-token window: two at
        // a time. Other Barry's window is unknown: all at once.
        let clients = IdentityClients {
            barry: client.clone(),
            other_barry: client.clone(),
            other_other_barry: client.clone(),
            judge: client,
            barry_max_tokens: 1000,
            other_barry_max_tokens: 1000,
            other_other_barry_max_tokens: 1000,
            judge_max_tokens: 1000,
            barry_context_size: Some(2500),
            other_barry_context_size: None,
            other_other_barry_context_size: None,
        };
        // Tiny prompts, so the estimate is all `max_tokens` and the arithmetic
        // is legible: four personas at ~1000 tokens each into a 2500-token
        // window is two at a time.
        let personas: Vec<Persona> = ["a", "b", "c", "d"]
            .into_iter()
            .map(|n| Persona {
                name: n,
                prompt: Arc::new(format!("you are {n}")),
            })
            .collect();
        let files = vec![ChangedFile {
            filename: "a.rs".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 0,
            changes: 1,
            patch: Some("+x".into()),
        }];
        let o = Orchestrator {
            clients: &clients,
            personas: &personas,
            tracker: Arc::new(StatusTracker::new()),
            job_id: 1,
        };
        o.run_persona_drafts(Identity::Barry, &files).await.unwrap();
        assert_eq!(peak.load(SeqCst), 2, "two personas in flight at most");

        // With no window known, all four go at once.
        peak.store(0, SeqCst);
        o.run_persona_drafts(Identity::OtherBarry, &files)
            .await
            .unwrap();
        assert_eq!(peak.load(SeqCst), 4);
    }

    #[test]
    fn the_gate_never_exceeds_the_persona_count() {
        assert_eq!(persona_concurrency(Some(1_000_000), 16, 0, 4), 4);
        assert_eq!(persona_concurrency(Some(1_000_000), 16, 0, 0), 1);
    }
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
            barry_context_size: None,
            other_barry_context_size: None,
            other_other_barry_context_size: None,
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
        // Two drafts, a first review and a revised one per reviewer.
        let c = clients(
            vec![Ok(approve()), Ok(approve()), Ok(approve()), Ok(approve())],
            vec![Ok(comment()), Ok(comment()), Ok(comment()), Ok(comment())],
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
    async fn the_judge_sees_the_second_round_not_the_first() {
        // Per reviewer, popped last-first: two drafts, R1, then R2 with the
        // other's R1 in hand. The judge must be shown the R2s.
        let c = clients(
            vec![
                Ok(ok_resp(
                    r#"{"outcome":"approve","summary":"barry revised","findings":[]}"#,
                )),
                Ok(comment()),
                Ok(approve()),
                Ok(approve()),
            ],
            vec![
                Ok(ok_resp(
                    r#"{"outcome":"approve","summary":"ob revised","findings":[]}"#,
                )),
                Ok(comment()),
                Ok(approve()),
                Ok(approve()),
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
            Verdict::Agree { barry } => assert_eq!(barry.summary, "barry revised"),
            other => panic!("wanted Agree, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_failed_second_round_keeps_that_reviewers_first() {
        let c = clients(
            vec![
                Err("r2 down"),
                Ok(ok_resp(
                    r#"{"outcome":"comment","summary":"barry first","findings":[]}"#,
                )),
                Ok(approve()),
                Ok(approve()),
            ],
            vec![
                Ok(ok_resp(
                    r#"{"outcome":"comment","summary":"ob revised","findings":[]}"#,
                )),
                Ok(comment()),
                Ok(approve()),
                Ok(approve()),
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
                barry, other_barry, ..
            } => {
                assert_eq!(barry.summary, "barry first");
                assert_eq!(other_barry.summary, "ob revised");
            }
            other => panic!("wanted Disagree, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_synthesis_json_falls_back_to_the_drafts() {
        // Barry: two drafts, then two malformed syntheses (the retry inside
        // synthesize), then nothing else: the review is built from the drafts.
        let bad = || ok_resp(r#"{"outcome":"approve","summary":"unterminated,"findings":[]}"#);
        let c = clients(
            vec![Ok(bad()), Ok(bad()), Ok(approve()), Ok(approve())],
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
                assert_eq!(reason, "synthesis produced invalid JSON twice");
                assert!(barry.summary.contains("unreconciled"), "{}", barry.summary);
            }
            other => panic!("wanted BarryAlone, got {other:?}"),
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
