//! Multi-identity, multi-persona LLM review checker.

pub mod clients;
pub mod confer;
pub mod identity;
pub mod judge;
pub mod orchestrator;
pub(super) mod parse_util;
pub mod persona;
pub mod posting;
pub mod review;
pub mod synthesis;

use crate::checker::multi_review::clients::IdentityClients;
use crate::checker::multi_review::identity::Identity;
use crate::checker::multi_review::orchestrator::{Orchestrator, Verdict};
use crate::checker::multi_review::persona::Persona;
use crate::checker::multi_review::posting::post_review;
use crate::checker::{Checker, CheckerCtx, CheckerOutcome, OutcomeStatus};
use crate::config::repo::RepoConfig;
use crate::dispatcher::run::{GhFactoryError, MultiGhFactory};
use crate::telemetry::status::StatusTracker;
use async_trait::async_trait;
use std::sync::Arc;

pub const CHECKER_NAME: &str = "barry/llm-review";

pub struct MultiReviewChecker {
    pub clients: Arc<IdentityClients>,
    pub personas: Arc<Vec<Persona>>,
    pub gh_factory: Arc<dyn MultiGhFactory>,
    pub status_tracker: Arc<StatusTracker>,
    /// When set, the reviewers run on the rack instead of against the
    /// configured LLM endpoints. The judge still runs here — it is a small
    /// remote model and reconciling two finished reviews needs no GPU.
    pub rack: Option<Arc<crate::rack::RackConfig>>,
    /// Used only for talking to systemslab, and only when `rack` is set.
    pub http: reqwest::Client,
    /// Fired when barry is stopping. A rack review dropped for that reason
    /// leaves its experiment running for the next process to resume (#39),
    /// where one dropped for any other reason cancels it.
    pub shutdown: tokio_util::sync::CancellationToken,
    /// Whether Other Other Barry breaks a disagreement. From
    /// [`crate::config::Config::tie_break_configured`].
    pub tie_break: bool,
    /// `[dispatcher] checker_timeout_secs`, which bounds [`Checker::run`].
    /// The tie-break stops short of it so that a slow Other Other Barry
    /// fails the tie-break, not the whole check.
    pub checker_timeout: std::time::Duration,
    /// Time kept back from the checker timeout for posting the tie-break
    /// review. [`post_reserve`] in production.
    pub post_reserve: std::time::Duration,
}

/// What came of handing a disagreement to Other Other Barry.
enum TieBreak {
    /// Other Other Barry reviewed and posted; its outcome decides the check.
    Decided(review::UnifiedReview),
    /// It was attempted and produced nothing that was posted. A fixed
    /// category, because it is published in the check-run summary; the
    /// error behind it is logged.
    Failed(&'static str),
}

/// How far a job's review got, kept on the job row so that a job handed
/// back by a stopping process resumes instead of starting again.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Progress {
    /// The head the verdict is for. A record for any other head is ignored.
    head_sha: String,
    verdict: Verdict,
    /// Identities whose review of this verdict is posted.
    posted: Vec<Identity>,
    /// Other Other Barry's review, once posted as the tie-break.
    tie_break: Option<review::UnifiedReview>,
}

/// How long posting the tie-break review can take. After the review, the
/// tie-break re-checks the head (an installation token and a PR lookup, as
/// Barry), mints Other Other Barry's token, and creates the review, which
/// retries for up to [`crate::github::client::WriteRetry`]'s deadline, then
/// makes one last attempt and a lookup for a copy that landed. GitHub's
/// client gives each request up to 30 s.
pub fn post_reserve() -> std::time::Duration {
    const REQUEST: std::time::Duration = std::time::Duration::from_secs(30);
    crate::github::client::WriteRetry::default().deadline + 3 * REQUEST
}

/// When the tie-break must stop, from when the checker started.
#[derive(Debug, Clone, Copy)]
struct TieBreakDeadlines {
    /// Other Other Barry's review, with the installation lookup and head
    /// check before it, must be done by this: the post reserve or a tenth of
    /// the checker timeout, whichever is longer, before the dispatcher's own
    /// timeout, so the review can still be posted and the check still says
    /// why there was no consensus.
    review: tokio::time::Instant,
    /// The dispatcher's timeout. The review is posted only if the post
    /// reserve is left before it.
    checker: tokio::time::Instant,
    post_reserve: std::time::Duration,
}

impl TieBreakDeadlines {
    fn new(
        start: std::time::Instant,
        checker_timeout: std::time::Duration,
        post_reserve: std::time::Duration,
    ) -> Self {
        let start = tokio::time::Instant::from_std(start);
        let checker = start + checker_timeout;
        let kept_back = post_reserve.max(checker_timeout / 10);
        Self {
            review: checker.checked_sub(kept_back).unwrap_or(start).max(start),
            checker,
            post_reserve,
        }
    }

    fn time_to_post(&self, now: tokio::time::Instant) -> bool {
        now + self.post_reserve <= self.checker
    }
}

impl MultiReviewChecker {
    /// Produce a verdict from reviews run on the rack.
    ///
    /// Both reviewer identities get their review from an ephemeral GPU guest.
    /// The judge runs there too when `[rack] judge` says so, against the model
    /// already loaded; otherwise it runs here against `[llm.judge]`, and a
    /// guest that was asked to judge but could not falls back to the same
    /// place.
    ///
    /// When Other Barry is not installed on the repo its reviewer is dropped
    /// before submitting. Running it anyway would spend a GPU host producing a
    /// review that cannot be posted. Dropping it also drops the in-guest judge:
    /// one review has nothing to be reconciled with.
    async fn run_on_rack(
        &self,
        rack: &Arc<crate::rack::RackConfig>,
        orchestrator: &Orchestrator<'_>,
        ctx: &CheckerCtx,
        ob_available: bool,
    ) -> anyhow::Result<Verdict> {
        let barry_slug = Identity::Barry.slug();
        let ob_slug = Identity::OtherBarry.slug();

        let mut cfg = (**rack).clone();
        if !ob_available {
            cfg.reviewers.retain(|r| r.identity != ob_slug);
        }
        if !cfg.reviewers.iter().any(|r| r.identity == barry_slug) {
            anyhow::bail!("rack is configured but has no `{barry_slug}` reviewer");
        }

        self.status_tracker.set_phase(ctx.job_id, "rack review");
        let name = format!("barry review {}/{} #{}", ctx.owner, ctx.repo, ctx.pr.number);
        if !ob_available {
            // Nothing to reconcile, so do not pay for a reconciliation.
            cfg.judge = false;
        }
        let outcome = crate::rack::review_for_job(
            &cfg,
            &self.http,
            &ctx.files,
            &name,
            &ctx.store,
            ctx.job_id,
            &self.shutdown,
        )
        .await?;
        let mut reviews = outcome.reviews;

        let barry = reviews
            .remove(barry_slug)
            .ok_or_else(|| anyhow::anyhow!("rack returned no review for {barry_slug}"))?;

        match reviews.remove(ob_slug) {
            Some(other) => match outcome.verdict {
                Some(v) => Ok(orchestrator.verdict_from(barry, other, v.agree, v.reason)),
                None => Ok(orchestrator.judge_reviews(barry, other).await),
            },
            None => Ok(Verdict::BarryAlone {
                barry,
                reason: if ob_available {
                    "Other Barry has no rack reviewer configured".into()
                } else {
                    "Other Barry not installed".into()
                },
            }),
        }
    }

    /// Have Other Other Barry review the head Barry and Other Barry disagreed
    /// on, post that review, and record it against the head and in the job's
    /// progress.
    ///
    /// The review is the one `/barry confer` runs: OOB reads the diff, not the
    /// two posted reviews. Recording the post means a later confer on this
    /// head finds every Barry posted and does not run OOB again; it is not
    /// counted against `[confer] max_per_pr`.
    ///
    /// Fails with "timed out" when the review is not done by
    /// `deadlines.review`, or when less than the post reserve is left to post
    /// it before the checker times out.
    ///
    /// `None` when OOB is not installed on the repo, which leaves the
    /// disagreement as it was without spending tokens on a review that cannot
    /// be posted, and when the PR's head moved or it closed, before the
    /// review or before its post: a push whose delivery was late or lost does
    /// not cancel the review, and a tie-break on an outdated commit is not
    /// worth its tokens or its post.
    async fn break_tie(
        &self,
        ctx: &CheckerCtx,
        key: crate::storage::multi_review::RunKey,
        reason: &str,
        deadlines: TieBreakDeadlines,
        progress: &mut Progress,
    ) -> Option<TieBreak> {
        let oob = Identity::OtherOtherBarry;
        let timed_out = || {
            tracing::warn!("tie-break: timed out");
            Some(TieBreak::Failed("timed out"))
        };
        // `timeout_at` polls the review once before it looks at the clock, so
        // a deadline already past is checked here, before any request.
        if tokio::time::Instant::now() >= deadlines.review {
            return timed_out();
        }
        let review = tokio::time::timeout_at(deadlines.review, async {
            match self
                .gh_factory
                .preflight_identity(oob, &ctx.owner, &ctx.repo)
                .await
            {
                Ok(()) => {}
                Err(GhFactoryError::NotInstalled { .. }) => {
                    tracing::info!("Other Other Barry not installed; the disagreement stands");
                    return None;
                }
                Err(GhFactoryError::Other(e)) => {
                    tracing::warn!(error = ?e, "tie-break: installation lookup failed");
                    return Some(Err("installation lookup failed"));
                }
            }
            if !self.head_is_current(ctx).await {
                return None;
            }
            tracing::info!("Barry and Other Barry disagree; Other Other Barry breaks the tie");
            self.status_tracker.set_phase(ctx.job_id, "tie-break");
            let diff = synthesis::render_diff_block(&ctx.files);
            match confer::run_unified(&self.clients, oob, &self.personas, &diff).await {
                Ok(r) => Some(Ok(r)),
                Err(e) => {
                    tracing::warn!(error = %e, "tie-break: Other Other Barry's review failed");
                    Some(Err(e.category()))
                }
            }
        });
        let (review, tokens) = match review.await {
            Ok(None) => return None,
            Ok(Some(Ok(r))) => r,
            Ok(Some(Err(category))) => return Some(TieBreak::Failed(category)),
            Err(_) => return timed_out(),
        };
        self.status_tracker
            .add_tokens(ctx.job_id, tokens.input, tokens.output);
        if !deadlines.time_to_post(tokio::time::Instant::now()) {
            return timed_out();
        }
        if !self.head_is_current(ctx).await {
            return None;
        }

        let split = format!("Barry and Other Barry disagreed: {reason}");
        if let Err(e) = post_review(
            &self.gh_factory,
            &ctx.owner,
            &ctx.repo,
            oob,
            ctx.pr.number,
            &ctx.pr.head.sha,
            &ctx.files,
            &review,
            Some(&split),
            true,
        )
        .await
        {
            tracing::warn!(error = %e, "tie-break: could not post Other Other Barry's review");
            return Some(TieBreak::Failed("could not post"));
        }
        progress.tie_break = Some(review.clone());
        self.save_progress(ctx, progress).await;
        let _ = ctx
            .store
            .record_post(key, oob, outcome_str(review.outcome), now_ts())
            .await;
        Some(TieBreak::Decided(review))
    }

    /// Barry and Other Barry's verdict on the head, from the rack or the
    /// configured endpoints.
    async fn verdict(
        &self,
        orchestrator: &Orchestrator<'_>,
        ctx: &CheckerCtx,
    ) -> anyhow::Result<Verdict> {
        // Pre-check: if OB isn't installed on this repo, skip OB's LLM phases
        // entirely and run Barry alone. This avoids paying OB's token cost
        // when we already know we can't post under that identity.
        let ob_available = match self
            .gh_factory
            .preflight_identity(Identity::OtherBarry, &ctx.owner, &ctx.repo)
            .await
        {
            Ok(()) => true,
            Err(GhFactoryError::NotInstalled { .. }) => false,
            Err(GhFactoryError::Other(e)) => return Err(e),
        };
        match &self.rack {
            Some(rack) => {
                self.run_on_rack(rack, orchestrator, ctx, ob_available)
                    .await
            }
            None if ob_available => orchestrator.run(&ctx.files).await,
            None => {
                orchestrator
                    .run_barry_only(&ctx.files, "Other Barry not installed".into())
                    .await
            }
        }
    }

    /// The progress this job recorded for the head under review, if any.
    async fn load_progress(&self, ctx: &CheckerCtx) -> Option<Progress> {
        let text = match ctx.store.review_progress(ctx.job_id).await {
            Ok(t) => t?,
            Err(e) => {
                tracing::warn!(error = %e, "could not read the job's review progress; starting over");
                return None;
            }
        };
        match serde_json::from_str::<Progress>(&text) {
            Ok(p) if p.head_sha == ctx.pr.head.sha => Some(p),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(error = %e, "unreadable review progress; starting over");
                None
            }
        }
    }

    /// Record `progress` on the job. A failure is logged and the review
    /// carries on: losing the record costs a repeat post after a restart,
    /// where failing here would cost the review.
    async fn save_progress(&self, ctx: &CheckerCtx, progress: &Progress) {
        let text = match serde_json::to_string(progress) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "could not encode the review progress");
                return;
            }
        };
        if let Err(e) = ctx.store.set_review_progress(ctx.job_id, &text).await {
            tracing::warn!(error = %e, "could not record the review progress");
        }
    }

    /// Post `review` as `identity` unless this job already has, and record
    /// that it has.
    async fn post_once(
        &self,
        ctx: &CheckerCtx,
        progress: &mut Progress,
        identity: Identity,
        review: &review::UnifiedReview,
        peer_disagreement: Option<&str>,
    ) -> Result<(), GhFactoryError> {
        if progress.posted.contains(&identity) {
            tracing::info!(?identity, "already posted by this job; not posting again");
            return Ok(());
        }
        post_review(
            &self.gh_factory,
            &ctx.owner,
            &ctx.repo,
            identity,
            ctx.pr.number,
            &ctx.pr.head.sha,
            &ctx.files,
            review,
            peer_disagreement,
            false,
        )
        .await?;
        progress.posted.push(identity);
        self.save_progress(ctx, progress).await;
        Ok(())
    }

    /// Whether the head under review is still the PR's head, asked as Barry.
    /// A lookup that fails counts as current, as it does before any post.
    async fn head_is_current(&self, ctx: &CheckerCtx) -> bool {
        let gh = match self
            .gh_factory
            .for_identity(Identity::Barry, &ctx.owner, &ctx.repo)
            .await
        {
            Ok(gh) => gh,
            Err(e) => {
                tracing::warn!(error = %e, "could not re-check the head; continuing");
                return true;
            }
        };
        crate::dispatcher::run::head_is_current(
            &gh,
            &ctx.owner,
            &ctx.repo,
            ctx.pr.number,
            &ctx.pr.head.sha,
        )
        .await
    }
}

#[async_trait]
impl Checker for MultiReviewChecker {
    fn name(&self) -> &'static str {
        CHECKER_NAME
    }
    fn enabled(&self, cfg: &RepoConfig) -> bool {
        cfg.multi_review.enabled
    }

    async fn run(&self, ctx: &CheckerCtx) -> anyhow::Result<CheckerOutcome> {
        let span = tracing::info_span!(
            "multi_review.checker",
            pr = ctx.pr.number,
            owner = %ctx.owner,
            repo = %ctx.repo,
            files = ctx.files.len(),
            head_sha = %ctx.pr.head.sha
        );
        let _enter = span.enter();

        tracing::info!("multi-review checker starting");
        let start = std::time::Instant::now();
        let orchestrator = Orchestrator {
            clients: &self.clients,
            personas: &self.personas,
            tracker: self.status_tracker.clone(),
            job_id: ctx.job_id,
        };

        // A job handed back by a stopping process resumes from what it had
        // done: its verdict, and which reviews it had posted. Without this a
        // restart during the tie-break would post Barry and Other Barry again.
        let verdict_kind = |v: &Verdict| match v {
            Verdict::Agree { .. } => "agree",
            Verdict::Disagree { .. } => "disagree",
            Verdict::BarryAlone { .. } => "barry_alone",
        };
        let mut progress = match self.load_progress(ctx).await {
            Some(p) => {
                tracing::info!(
                    verdict = verdict_kind(&p.verdict),
                    posted = ?p.posted,
                    "resuming the review this job had begun"
                );
                p
            }
            None => {
                let verdict = self.verdict(&orchestrator, ctx).await?;
                tracing::info!(
                    verdict = verdict_kind(&verdict),
                    orchestrator_duration_ms = start.elapsed().as_millis() as u64,
                    "orchestrator verdict received"
                );
                let p = Progress {
                    head_sha: ctx.pr.head.sha.clone(),
                    verdict,
                    posted: vec![],
                    tie_break: None,
                };
                self.save_progress(ctx, &p).await;
                p
            }
        };

        // Post under each Barry that has something to say.
        match progress.verdict.clone() {
            Verdict::Agree { barry } | Verdict::BarryAlone { barry, .. } => {
                self.post_once(ctx, &mut progress, Identity::Barry, &barry, None)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
            }
            Verdict::Disagree {
                barry,
                other_barry,
                reason,
            } => {
                let disagreement_msg = format!("I disagree with Barry: {reason}");
                self.post_once(ctx, &mut progress, Identity::Barry, &barry, None)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                // OB post: NotInstalled here means OB was uninstalled between
                // our pre-check and now. Downgrade to BarryAlone bookkeeping.
                match self
                    .post_once(
                        ctx,
                        &mut progress,
                        Identity::OtherBarry,
                        &other_barry,
                        Some(&disagreement_msg),
                    )
                    .await
                {
                    Ok(()) => {}
                    Err(GhFactoryError::NotInstalled { .. }) => {
                        tracing::warn!("OB uninstalled mid-run; downgrading to BarryAlone");
                        metrics::counter!("barry_multi_review_barry_alone_total").increment(1);
                        progress.verdict = Verdict::BarryAlone {
                            barry,
                            reason: "Other Barry uninstalled mid-run".into(),
                        };
                        self.save_progress(ctx, &progress).await;
                    }
                    Err(GhFactoryError::Other(e)) => return Err(e),
                }
            }
        }
        let verdict = progress.verdict.clone();

        // Persist run state. Recorded under Barry's installation, and before
        // any tie-break: a cancel during the tie-break drops this future, and
        // the reviews already posted must still be on record.
        let now = now_ts();
        let key = crate::storage::multi_review::RunKey {
            owner: ctx.owner.clone(),
            repo: ctx.repo.clone(),
            pr: ctx.pr.number,
            head_sha: ctx.pr.head.sha.clone(),
        };
        match &verdict {
            Verdict::Agree { barry } | Verdict::BarryAlone { barry, .. } => {
                let _ = ctx
                    .store
                    .record_post(
                        key.clone(),
                        Identity::Barry,
                        outcome_str(barry.outcome),
                        now,
                    )
                    .await;
            }
            Verdict::Disagree {
                barry, other_barry, ..
            } => {
                let _ = ctx
                    .store
                    .record_post(
                        key.clone(),
                        Identity::Barry,
                        outcome_str(barry.outcome),
                        now,
                    )
                    .await;
                let _ = ctx
                    .store
                    .record_post(
                        key.clone(),
                        Identity::OtherBarry,
                        outcome_str(other_barry.outcome),
                        now,
                    )
                    .await;
            }
        }

        // A disagreement goes to Other Other Barry, whose outcome then decides
        // the check. Runs inside this future, so the checker timeout and a
        // cancel stop it as they stop the rest of the review.
        let tie_break = match (&verdict, progress.tie_break.clone()) {
            (Verdict::Disagree { .. }, Some(posted)) => {
                // The job may have stopped between saving this and recording
                // it against the head; unrecorded, a later confer would run
                // OOB again. record_post is an upsert.
                let _ = ctx
                    .store
                    .record_post(
                        key,
                        Identity::OtherOtherBarry,
                        outcome_str(posted.outcome),
                        now_ts(),
                    )
                    .await;
                Some(TieBreak::Decided(posted))
            }
            (Verdict::Disagree { reason, .. }, None) if self.tie_break => {
                let deadlines =
                    TieBreakDeadlines::new(start, self.checker_timeout, self.post_reserve);
                let t = self
                    .break_tie(ctx, key, reason, deadlines, &mut progress)
                    .await;
                match &t {
                    Some(TieBreak::Decided(oob)) => {
                        tracing::info!(outcome = ?oob.outcome, "tie-break decided");
                        metrics::counter!("barry_multi_review_tie_break_total", "outcome" => outcome_str(oob.outcome)).increment(1);
                    }
                    Some(TieBreak::Failed(e)) => {
                        tracing::warn!(category = e, "tie-break failed; the disagreement stands");
                        metrics::counter!("barry_multi_review_tie_break_total", "outcome" => "failed")
                            .increment(1);
                    }
                    None => {}
                }
                t
            }
            _ => None,
        };

        // Return the dispatcher-style outcome that drives the check-run.
        let check_outcome = match &tie_break {
            Some(TieBreak::Decided(oob)) => oob.outcome,
            _ => verdict.check_outcome(),
        };
        let status = match check_outcome {
            review::Outcome::Approve => OutcomeStatus::Success,
            review::Outcome::Comment => OutcomeStatus::Neutral,
            review::Outcome::RequestChanges => OutcomeStatus::Failure,
        };
        let summary = match (&verdict, &tie_break) {
            (Verdict::Agree { barry }, _) => format!("Barry — {}", first_line(&barry.summary)),
            (Verdict::BarryAlone { barry, .. }, _) => {
                format!("Barry (alone) — {}", first_line(&barry.summary))
            }
            (
                Verdict::Disagree {
                    barry,
                    other_barry,
                    reason,
                },
                Some(TieBreak::Decided(oob)),
            ) => format!(
                "Tie-break by Other Other Barry ({:?}) — {}. The split: Barry {:?}, Other Barry {:?}: {reason}",
                oob.outcome,
                first_line(&oob.summary).trim_end_matches('.'),
                barry.outcome,
                other_barry.outcome,
            ),
            (Verdict::Disagree { reason, .. }, Some(TieBreak::Failed(e))) => {
                format!("No consensus: {reason} (tie-break failed: {e})")
            }
            (Verdict::Disagree { reason, .. }, None) => format!("No consensus: {reason}"),
        };
        tracing::info!(?status, "multi-review checker done");
        Ok(CheckerOutcome {
            checker_name: CHECKER_NAME,
            status,
            summary,
            text: None,
            inline_comments: vec![],
            issue_comment: None,
            add_labels: vec![],
        })
    }
}

fn outcome_str(o: review::Outcome) -> &'static str {
    match o {
        review::Outcome::Approve => "approve",
        review::Outcome::Comment => "comment",
        review::Outcome::RequestChanges => "request_changes",
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(140).collect()
}

use crate::util::now_ts;

#[cfg(test)]
mod checker_tests {
    use super::*;
    use crate::checker::multi_review::identity::Identity;
    use crate::dispatcher::run::GhFactoryError;
    use async_trait::async_trait;
    use std::sync::Arc;

    struct StubFactory {
        ob_installed: bool,
    }

    #[async_trait]
    impl crate::dispatcher::run::GhFactory for StubFactory {
        async fn for_installation(
            &self,
            _installation_id: i64,
        ) -> anyhow::Result<Arc<crate::github::client::GitHub>> {
            anyhow::bail!("not used in this test")
        }
    }

    #[async_trait]
    impl crate::dispatcher::run::MultiGhFactory for StubFactory {
        async fn for_identity(
            &self,
            identity: Identity,
            owner: &str,
            repo: &str,
        ) -> Result<Arc<crate::github::client::GitHub>, GhFactoryError> {
            if identity == Identity::OtherBarry && !self.ob_installed {
                return Err(GhFactoryError::NotInstalled {
                    identity,
                    owner: owner.to_string(),
                    repo: repo.to_string(),
                });
            }
            Err(GhFactoryError::Other(anyhow::anyhow!(
                "for_identity should not be reached in preflight-only tests"
            )))
        }

        async fn preflight_identity(
            &self,
            identity: Identity,
            owner: &str,
            repo: &str,
        ) -> Result<(), GhFactoryError> {
            if identity == Identity::OtherBarry && !self.ob_installed {
                return Err(GhFactoryError::NotInstalled {
                    identity,
                    owner: owner.to_string(),
                    repo: repo.to_string(),
                });
            }
            Ok(())
        }
    }

    #[test]
    fn the_tie_break_review_keeps_back_the_post_reserve_or_a_tenth_of_the_timeout() {
        use std::time::Duration;
        let start = std::time::Instant::now();
        let at = |secs| tokio::time::Instant::from_std(start) + Duration::from_secs(secs);
        let t = Duration::from_secs(600);

        // The default reserve is longer than a tenth of the default timeout.
        assert_eq!(post_reserve(), Duration::from_secs(390));
        let d = TieBreakDeadlines::new(start, t, post_reserve());
        assert_eq!(d.review, at(210));
        assert_eq!(d.checker, at(600));
        assert!(d.time_to_post(at(210)));
        assert!(!d.time_to_post(at(211)));

        // A short reserve: a tenth of the timeout is kept back instead.
        let d = TieBreakDeadlines::new(start, t, Duration::from_secs(30));
        assert_eq!(d.review, at(540));
        assert!(d.time_to_post(at(570)));

        // A reserve longer than the timeout leaves no time to review.
        let d = TieBreakDeadlines::new(start, Duration::from_secs(60), post_reserve());
        assert_eq!(d.review, at(0));
    }

    /// A record written by the deployed version must still decode after an
    /// upgrade: one that does not is ignored, and the job then reviews and
    /// posts again. A change to `Verdict`, `UnifiedReview` or `Identity` that
    /// breaks this needs a migration, not an edit to the fixture.
    #[test]
    fn a_recorded_review_progress_still_decodes() {
        let p: Progress =
            serde_json::from_str(include_str!("../../../tests/fixtures/review_progress.json"))
                .unwrap();
        assert_eq!(p.head_sha, "sha1");
        assert_eq!(p.posted, vec![Identity::Barry, Identity::OtherBarry]);
        match p.verdict {
            Verdict::Disagree {
                barry,
                other_barry,
                reason,
            } => {
                assert_eq!(barry.outcome, review::Outcome::RequestChanges);
                assert_eq!(barry.findings[0].line, 1);
                assert_eq!(other_barry.outcome, review::Outcome::Comment);
                assert_eq!(reason, "they weigh the unwrap differently");
            }
            other => panic!("wanted Disagree, got {other:?}"),
        }
        assert_eq!(p.tie_break.unwrap().outcome, review::Outcome::Approve);
    }

    #[tokio::test]
    async fn stub_factory_compiles() {
        let f = StubFactory {
            ob_installed: false,
        };
        let err = f
            .preflight_identity(Identity::OtherBarry, "acme", "widget")
            .await
            .unwrap_err();
        assert!(matches!(err, GhFactoryError::NotInstalled { .. }));
    }
}

#[cfg(test)]
mod rack_tests {
    use crate::checker::multi_review::identity::Identity;
    use crate::rack::RackConfig;

    fn cfg() -> RackConfig {
        toml::from_str(
            r#"
systemslab = "http://systemslab"
host_tags = ["z2.baremetal"]
shape = "z2.g.medium"
image = "img"

[[reviewers]]
identity = "barry"
model = "m1"
model_name = "n1"

[[reviewers]]
identity = "other_barry"
model = "m2"
model_name = "n2"
"#,
        )
        .unwrap()
    }

    #[test]
    fn reviewer_identities_match_the_identity_slugs() {
        // run_on_rack looks reviews up by slug. If a config used a different
        // spelling the reviews would be produced, paid for on a GPU, and then
        // not found.
        let c = cfg();
        assert!(
            c.reviewers
                .iter()
                .any(|r| r.identity == Identity::Barry.slug())
        );
        assert!(
            c.reviewers
                .iter()
                .any(|r| r.identity == Identity::OtherBarry.slug())
        );
    }

    #[test]
    fn dropping_other_barry_leaves_a_runnable_config() {
        // When OB is not installed on the repo its reviewer is dropped before
        // submitting, rather than spending a GPU host on a review that cannot
        // be posted.
        let mut c = cfg();
        c.reviewers
            .retain(|r| r.identity != Identity::OtherBarry.slug());
        assert_eq!(c.reviewers.len(), 1);
        assert_eq!(c.reviewers[0].identity, Identity::Barry.slug());
        // Still a valid experiment: something to run, and an artifact to fetch.
        assert!(
            crate::rack::job::spec(
                &c,
                &[crate::github::pr::ChangedFile {
                    filename: "a".into(),
                    status: "modified".into(),
                    additions: 1,
                    deletions: 0,
                    changes: 1,
                    patch: Some("@@".into()),
                }],
                "t"
            )
            .is_ok()
        );
    }

    #[test]
    fn a_config_without_barry_is_rejected_before_submitting() {
        // Barry is the identity that posts. A rack config that cannot produce
        // his review has nothing useful to run, and finding that out after a
        // GPU job is the expensive way to learn it.
        let mut c = cfg();
        c.reviewers.retain(|r| r.identity != Identity::Barry.slug());
        assert!(
            !c.reviewers
                .iter()
                .any(|r| r.identity == Identity::Barry.slug())
        );
    }
}
