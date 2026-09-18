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
        let mut verdict = match &self.rack {
            Some(rack) => {
                self.run_on_rack(rack, &orchestrator, ctx, ob_available)
                    .await?
            }
            None if ob_available => orchestrator.run(&ctx.files).await?,
            None => {
                orchestrator
                    .run_barry_only(&ctx.files, "Other Barry not installed".into())
                    .await?
            }
        };
        let orchestrator_duration = start.elapsed();

        let verdict_kind = match &verdict {
            Verdict::Agree { .. } => "agree",
            Verdict::Disagree { .. } => "disagree",
            Verdict::BarryAlone { .. } => "barry_alone",
        };
        tracing::info!(
            verdict = verdict_kind,
            orchestrator_duration_ms = orchestrator_duration.as_millis() as u64,
            "orchestrator verdict received"
        );

        // Post under each Barry that has something to say.
        match &verdict {
            Verdict::Agree { barry } | Verdict::BarryAlone { barry, .. } => {
                post_review(
                    &self.gh_factory,
                    &ctx.owner,
                    &ctx.repo,
                    Identity::Barry,
                    ctx.pr.number,
                    &ctx.pr.head.sha,
                    &ctx.files,
                    barry,
                    None,
                )
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            }
            Verdict::Disagree {
                barry,
                other_barry,
                reason,
            } => {
                let disagreement_msg = format!("I disagree with Barry: {reason}");
                post_review(
                    &self.gh_factory,
                    &ctx.owner,
                    &ctx.repo,
                    Identity::Barry,
                    ctx.pr.number,
                    &ctx.pr.head.sha,
                    &ctx.files,
                    barry,
                    None,
                )
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
                // OB post: NotInstalled here means OB was uninstalled between
                // our pre-check and now. Downgrade to BarryAlone bookkeeping.
                let downgrade = match post_review(
                    &self.gh_factory,
                    &ctx.owner,
                    &ctx.repo,
                    Identity::OtherBarry,
                    ctx.pr.number,
                    &ctx.pr.head.sha,
                    &ctx.files,
                    other_barry,
                    Some(&disagreement_msg),
                )
                .await
                {
                    Ok(()) => false,
                    Err(GhFactoryError::NotInstalled { .. }) => {
                        tracing::warn!("OB uninstalled mid-run; downgrading to BarryAlone");
                        metrics::counter!("barry_multi_review_barry_alone_total").increment(1);
                        true
                    }
                    Err(GhFactoryError::Other(e)) => return Err(e),
                };
                if downgrade {
                    let barry_clone = barry.clone();
                    verdict = Verdict::BarryAlone {
                        barry: barry_clone,
                        reason: "Other Barry uninstalled mid-run".into(),
                    };
                }
            }
        }

        // Persist run state. Recorded under Barry's installation.
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
                    .record_post(key, Identity::Barry, outcome_str(barry.outcome), now)
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
                        key,
                        Identity::OtherBarry,
                        outcome_str(other_barry.outcome),
                        now,
                    )
                    .await;
            }
        }

        // Return the dispatcher-style outcome that drives the check-run.
        let status = match verdict.check_outcome() {
            review::Outcome::Approve => OutcomeStatus::Success,
            review::Outcome::Comment => OutcomeStatus::Neutral,
            review::Outcome::RequestChanges => OutcomeStatus::Failure,
        };
        let summary = match &verdict {
            Verdict::Agree { barry } => format!("Barry — {}", first_line(&barry.summary)),
            Verdict::BarryAlone { barry, .. } => {
                format!("Barry (alone) — {}", first_line(&barry.summary))
            }
            Verdict::Disagree { reason, .. } => format!("No consensus: {reason}"),
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
