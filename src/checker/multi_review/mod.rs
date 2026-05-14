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
use crate::dispatcher::run::MultiGhFactory;
use async_trait::async_trait;
use std::sync::Arc;

pub const CHECKER_NAME: &str = "barry/llm-review";

pub struct MultiReviewChecker {
    pub clients: Arc<IdentityClients>,
    pub personas: Arc<Vec<Persona>>,
    pub gh_factory: Arc<dyn MultiGhFactory>,
}

#[async_trait]
impl Checker for MultiReviewChecker {
    fn name(&self) -> String {
        CHECKER_NAME.to_string()
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
        let installation_id = installation_id_from_ctx(ctx)?;
        let start = std::time::Instant::now();
        let orchestrator = Orchestrator {
            clients: &self.clients,
            personas: &self.personas,
        };
        let verdict = orchestrator.run(&ctx.files).await?;
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
                    installation_id,
                    Identity::Barry,
                    &ctx.owner,
                    &ctx.repo,
                    ctx.pr.number,
                    &ctx.pr.head.sha,
                    &ctx.files,
                    barry,
                    None,
                )
                .await?;
            }
            Verdict::Disagree {
                barry,
                other_barry,
                reason,
            } => {
                let disagreement_msg = format!("I disagree with Barry: {reason}");
                post_review(
                    &self.gh_factory,
                    installation_id,
                    Identity::Barry,
                    &ctx.owner,
                    &ctx.repo,
                    ctx.pr.number,
                    &ctx.pr.head.sha,
                    &ctx.files,
                    barry,
                    None,
                )
                .await?;
                post_review(
                    &self.gh_factory,
                    installation_id,
                    Identity::OtherBarry,
                    &ctx.owner,
                    &ctx.repo,
                    ctx.pr.number,
                    &ctx.pr.head.sha,
                    &ctx.files,
                    other_barry,
                    Some(&disagreement_msg),
                )
                .await?;
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
            checker_name: CHECKER_NAME.to_string(),
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

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn installation_id_from_ctx(ctx: &CheckerCtx) -> anyhow::Result<i64> {
    ctx.installation_id
        .ok_or_else(|| anyhow::anyhow!("installation_id not available in CheckerCtx"))
}
