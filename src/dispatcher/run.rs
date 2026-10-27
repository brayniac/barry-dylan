use crate::checker::multi_review::identity::Identity;
use crate::checker::{Checker, CheckerCtx, CheckerOutcome, OutcomeStatus};
use crate::config::Config;
use crate::dispatcher::trust::{self, BarryCommand, Trust};
use crate::github::check_run::{CheckConclusion, CheckOutput, CheckRunInput, CheckStatus};
use crate::github::client::{GhError, GitHub};
use crate::github::pr::{BotComment, PullRequest, ReviewInput};
use crate::storage::Store;
use crate::storage::queue::LeasedJob;
use crate::telemetry::status::StatusTracker;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tracing::Instrument;

pub struct Pipeline {
    pub checkers: Vec<Arc<dyn Checker>>,
}

impl Pipeline {
    pub fn hygiene_only() -> Self {
        Self {
            checkers: vec![
                Arc::new(crate::checker::hygiene::title::TitleChecker),
                Arc::new(crate::checker::hygiene::description::DescriptionChecker),
                Arc::new(crate::checker::hygiene::size::SizeChecker),
                Arc::new(crate::checker::hygiene::autolabel::AutolabelChecker),
            ],
        }
    }
}

pub struct JobDeps {
    pub store: Store,
    pub config: Arc<Config>,
    pub pipeline: Arc<Pipeline>,
    pub gh_factory: Arc<dyn MultiGhFactory>,
    /// Shared per-identity LLM clients. None when the pipeline is hygiene-only
    /// (no multi-review configured) — confer is then a no-op.
    pub clients: Option<Arc<crate::checker::multi_review::clients::IdentityClients>>,
    /// Persona definitions used by multi-review and confer. None matches clients=None.
    pub personas: Option<Arc<Vec<crate::checker::multi_review::persona::Persona>>>,
    pub status_tracker: Arc<StatusTracker>,
    pub cancel_registry: crate::dispatcher::cancel::CancelRegistry,
}

#[async_trait::async_trait]
pub trait GhFactory: Send + Sync {
    async fn for_installation(&self, installation_id: i64) -> anyhow::Result<Arc<GitHub>>;
}

#[derive(Debug, thiserror::Error)]
pub enum GhFactoryError {
    #[error("{identity:?} is not installed on {owner}/{repo}")]
    NotInstalled {
        identity: Identity,
        owner: String,
        repo: String,
    },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[async_trait::async_trait]
pub trait MultiGhFactory: GhFactory {
    /// Mint a GitHub client authenticated as the given identity for the given
    /// repo. Internally resolves the installation_id for (identity, owner).
    /// Returns `Err(NotInstalled)` if the identity's App is not installed on
    /// the owner.
    async fn for_identity(
        &self,
        identity: Identity,
        owner: &str,
        repo: &str,
    ) -> Result<Arc<GitHub>, GhFactoryError>;

    /// Verify the identity's App is installed on the owner without minting a
    /// token. Same cache writes and WARN+metric side effects as `for_identity`.
    async fn preflight_identity(
        &self,
        identity: Identity,
        owner: &str,
        repo: &str,
    ) -> Result<(), GhFactoryError>;
}

pub async fn run_job(deps: &JobDeps, job: &LeasedJob) -> anyhow::Result<()> {
    let span = tracing::info_span!(
        "job.run",
        owner = %job.repo_owner,
        repo = %job.repo_name,
        pr = job.pr_number,
        event_kind = %job.event_kind,
        delivery_id = %job.delivery_id,
        installation_id = job.installation_id
    );
    let _enter = span.enter();

    tracing::info!("starting job processing");

    if job.event_kind == "pull_request.closed" {
        return handle_pr_closed(deps, job).await;
    }

    let gh = deps
        .gh_factory
        .for_installation(job.installation_id)
        .await?;

    if let Some(cmd) = parse_command_event(&job.event_kind) {
        tracing::info!(?cmd, "handling /barry command");
        return handle_command(deps, &gh, job, cmd).await;
    }

    // Setup phase: one GraphQL query (PR metadata + comments + reviews + .barry.toml blob)
    // and `list_pr_files` REST fired concurrently — the GraphQL files connection doesn't
    // expose unified-diff patches that the LLM checker needs.
    let (ctx_res, files_res) = tokio::join!(
        gh.fetch_pr_context(&job.repo_owner, &job.repo_name, job.pr_number),
        gh.list_pr_files(&job.repo_owner, &job.repo_name, job.pr_number),
    );
    let pr_ctx = ctx_res?;
    let files = files_res?;
    let pr = pr_ctx.pr;
    let cfg_text = pr_ctx.config_text;
    let prior_comments = pr_ctx.comments;
    let prior_reviews = pr_ctx.reviews;

    if pr.state != "open" {
        tracing::info!(state = %pr.state, "PR is no longer open; aborting review");
        return Ok(());
    }

    let perm = gh
        .author_permission(&job.repo_owner, &job.repo_name, &pr.user.login)
        .await
        .unwrap_or_else(|_| "read".into());
    let bot_comments: Vec<BotComment> = prior_comments
        .iter()
        .filter(|c| c.author.starts_with("barry-dylan"))
        .cloned()
        .collect();
    let bot_reviews: Vec<BotComment> = prior_reviews
        .iter()
        .filter(|c| c.author.starts_with("barry-dylan"))
        .cloned()
        .collect();

    // Trust gate.
    let trust_decision = trust::evaluate_trust(&perm, &bot_comments);
    tracing::info!(
        author = %pr.user.login,
        permission = %perm,
        trust = ?trust_decision,
        "trust decision made"
    );
    if trust_decision == Trust::NeedsApproval {
        post_needs_approval_once(&gh, job, &pr, &bot_comments).await?;
        return Ok(());
    }

    let repo_cfg = parse_repo_config_with_check(deps, &gh, job, &pr, cfg_text).await?;

    let ctx = CheckerCtx {
        gh: gh.clone(),
        repo_cfg: Arc::new(repo_cfg),
        owner: job.repo_owner.clone(),
        repo: job.repo_name.clone(),
        pr: Arc::new(pr),
        files,
        prior_bot_reviews: bot_reviews,
        prior_bot_comments: bot_comments,
        store: deps.store.clone(),
        installation_id: Some(job.installation_id),
        job_id: job.id,
    };

    let checker_timeout = Duration::from_secs(deps.config.dispatcher.checker_timeout_secs);
    let store = &deps.store;
    let gh_ref = &gh;
    let job_ref = job;
    let pr_ref = &ctx.pr;
    let ctx_ref = &ctx;
    let rate_limit_reset: Arc<AtomicI64> = Arc::new(AtomicI64::new(0));

    // Register a cancellation token for this PR and the head under review. A
    // concurrent `pull_request.closed` job fires it, and so does the head
    // watcher below when a push has moved the head, causing all checker tasks
    // to abort before posting results.
    let Some(registered) = deps
        .cancel_registry
        .register(
            &job.repo_owner,
            &job.repo_name,
            job.pr_number,
            &ctx.pr.head.sha,
        )
        .await
    else {
        tracing::info!(
            head = %ctx.pr.head.sha,
            "a review of this head is already in flight; not starting another"
        );
        metrics::counter!("barry_review_skipped_total", "reason" => "in_flight").increment(1);
        return Ok(());
    };
    let cancel = registered.token.clone();

    let gh_factory = deps.gh_factory.clone();
    let installation_id = job.installation_id;

    let mut tasks = Vec::new();
    for chk in &deps.pipeline.checkers {
        if !chk.enabled(&ctx.repo_cfg) {
            continue;
        }
        let chk = chk.clone();
        let gh_factory = gh_factory.clone();
        let rate_limit_reset = rate_limit_reset.clone();
        let checker_name = chk.name();
        let cancel = cancel.clone();
        let span = tracing::info_span!(
            "checker.run",
            checker = checker_name,
            pr = job_ref.pr_number,
            owner = %job_ref.repo_owner,
            repo = %job_ref.repo_name
        );
        tasks.push(
            async move {
                tracing::debug!("checker starting");
                let t = std::time::Instant::now();
                // Level 3: race checker execution against cancellation.
                let run_result = tokio::select! {
                    res = tokio::time::timeout(checker_timeout, chk.run(ctx_ref)) => Some(res),
                    _ = cancel.cancelled() => None,
                };
                let dur = t.elapsed();
                let outcome = match run_result {
                    None => {
                        tracing::info!(
                            checker = checker_name,
                            "checker cancelled: PR closed or head moved"
                        );
                        return;
                    }
                    Some(Ok(Ok(o))) => o,
                    Some(Ok(Err(e))) => {
                        tracing::error!(error = ?e, "checker failed");
                        // The summary is all a reader gets on the check run, and
                        // "internal error (see logs)" cannot distinguish "the rack
                        // was busy" from "barry is broken" -- which is how a real
                        // failure gets ignored. The error's own first line says
                        // which, so use it; the logs still have the rest.
                        CheckerOutcome::neutral(checker_name, summarize_error(&e))
                    }
                    Some(Err(_)) => {
                        let timeout_msg = format!("timed out after {}s", checker_timeout.as_secs());
                        tracing::warn!(timeout_msg, "checker timed out");
                        CheckerOutcome::neutral(checker_name, &timeout_msg)
                    }
                };
                // Level 1: cancel may have fired in the window between run() returning
                // and posting results.
                if cancel.is_cancelled() {
                    tracing::info!(checker = checker_name, "checker cancelled before posting");
                    return;
                }
                tracing::info!(
                    checker = checker_name,
                    status = status_str(outcome.status),
                    duration_ms = dur.as_millis() as u64,
                    "checker completed"
                );
                let post_gh = match gh_factory.for_installation(installation_id).await {
                    Ok(g) => g,
                    Err(e) => {
                        tracing::warn!(
                            ?e,
                            "failed to re-mint token before post_outcome; using original"
                        );
                        Arc::clone(gh_ref)
                    }
                };
                if let Err(e) = post_outcome(&post_gh, job_ref, pr_ref, &outcome, &cancel).await {
                    if let Some(GhError::RateLimited { reset_in_secs }) =
                        e.downcast_ref::<GhError>()
                    {
                        rate_limit_reset.fetch_max(*reset_in_secs, Ordering::SeqCst);
                        tracing::warn!(reset_in_secs, "post_outcome rate limited");
                    } else {
                        tracing::error!(error = ?e, "post_outcome failed");
                    }
                }
                let _ = store
                    .append_audit(&crate::storage::audit::AuditEntry {
                        ts: now_ts(),
                        delivery_id: Some(job_ref.delivery_id.clone()),
                        repo_owner: Some(job_ref.repo_owner.clone()),
                        repo_name: Some(job_ref.repo_name.clone()),
                        pr_number: Some(job_ref.pr_number),
                        checker_name: Some(checker_name.to_string()),
                        outcome: status_str(outcome.status).to_string(),
                        duration_ms: Some(dur.as_millis() as i64),
                        details: None,
                    })
                    .await;
            }
            .instrument(span),
        );
    }
    let checkers = futures::future::join_all(tasks);
    tokio::pin!(checkers);
    tokio::select! {
        _ = &mut checkers => {}
        _ = watch_head(
            gh_ref,
            deps.gh_factory.as_ref(),
            job_ref,
            &pr_ref.head.sha,
            &registered,
            RECHECK_INTERVAL,
        ) => {
            checkers.await;
        }
    }

    // Unregisters this review, unless a review of a newer head has since
    // replaced it. A panic or early return unregisters it the same way.
    drop(registered);

    let reset_in_secs = rate_limit_reset.load(Ordering::SeqCst);
    if reset_in_secs > 0 {
        return Err(GhError::RateLimited { reset_in_secs }.into());
    }
    Ok(())
}

async fn handle_pr_closed(deps: &JobDeps, job: &LeasedJob) -> anyhow::Result<()> {
    // Level 2: delete any queued (not yet leased) review jobs for this PR.
    deps.store
        .cancel_pr_jobs(&job.repo_owner, &job.repo_name, job.pr_number)
        .await?;
    // Level 3: signal any in-flight checker tasks to abort.
    deps.cancel_registry
        .cancel(&job.repo_owner, &job.repo_name, job.pr_number)
        .await;
    tracing::info!(
        owner = %job.repo_owner,
        repo = %job.repo_name,
        pr = job.pr_number,
        "PR closed: purged queue and signalled in-flight cancellation"
    );
    Ok(())
}

fn parse_command_event(kind: &str) -> Option<BarryCommand> {
    let sub = kind.strip_prefix("issue_comment.")?;
    Some(match sub {
        "approve" => BarryCommand::Approve,
        "review" => BarryCommand::Review,
        "confer" => BarryCommand::Confer,
        _ => BarryCommand::Unknown,
    })
}

async fn handle_command(
    deps: &JobDeps,
    gh: &Arc<GitHub>,
    job: &LeasedJob,
    cmd: BarryCommand,
) -> anyhow::Result<()> {
    let span = tracing::info_span!(
        "job.command",
        command = ?cmd,
        pr = job.pr_number,
        owner = %job.repo_owner,
        repo = %job.repo_name
    );
    let _enter = span.enter();

    match cmd {
        BarryCommand::Approve => {
            // Only maintainers (write/admin) can trust an untrusted PR author.
            // Authorize the issuer threaded through from the webhook.
            let Some(actor) = job.actor.clone() else {
                tracing::warn!("approve job missing actor field; ignoring");
                return Ok(());
            };
            let perm = gh
                .author_permission(&job.repo_owner, &job.repo_name, &actor)
                .await
                .unwrap_or_else(|_| "read".into());
            if !matches!(perm.as_str(), "admin" | "maintain" | "write") {
                tracing::info!(%actor, %perm, "approve rejected (unauthorized)");
                return Ok(());
            }
            tracing::info!(%actor, "approving PR for review");
            gh.create_issue_comment(
                &job.repo_owner,
                &job.repo_name,
                job.pr_number,
                &trust::approve_comment_body(),
            )
            .await?;
            // Re-enqueue a normal review job for this PR.
            let now = now_ts();
            deps.store
                .enqueue(
                    &crate::storage::queue::NewJob {
                        installation_id: job.installation_id,
                        repo_owner: job.repo_owner.clone(),
                        repo_name: job.repo_name.clone(),
                        pr_number: job.pr_number,
                        event_kind: "pull_request.opened".into(),
                        delivery_id: job.delivery_id.clone(),
                        actor: None,
                    },
                    now,
                    now,
                )
                .await?;
        }
        BarryCommand::Review => {
            tracing::info!("re-running review on current head");
            let now = now_ts();
            deps.store
                .enqueue(
                    &crate::storage::queue::NewJob {
                        installation_id: job.installation_id,
                        repo_owner: job.repo_owner.clone(),
                        repo_name: job.repo_name.clone(),
                        pr_number: job.pr_number,
                        event_kind: "pull_request.opened".into(),
                        delivery_id: job.delivery_id.clone(),
                        actor: None,
                    },
                    now,
                    now,
                )
                .await?;
        }
        BarryCommand::Confer => {
            crate::checker::multi_review::confer::handle(deps, gh, job).await?;
        }
        BarryCommand::Unknown | BarryCommand::NotACommand => {
            // Cannot react without the comment ID stored on the job.
            // For v1 we accept the limitation and just log.
            tracing::info!(?cmd, "unknown /barry command");
        }
    }
    Ok(())
}

async fn post_needs_approval_once(
    gh: &Arc<GitHub>,
    job: &LeasedJob,
    pr: &PullRequest,
    bot_comments: &[BotComment],
) -> anyhow::Result<()> {
    if bot_comments
        .iter()
        .any(|c| c.body.contains(trust::NEEDS_APPROVAL_MARKER))
    {
        tracing::debug!("needs-approval comment already posted");
        return Ok(());
    }
    gh.create_issue_comment(
        &job.repo_owner,
        &job.repo_name,
        job.pr_number,
        &trust::needs_approval_body(&pr.user.login),
    )
    .await?;
    tracing::info!("posted needs-approval comment for untrusted author");
    Ok(())
}

async fn parse_repo_config_with_check(
    deps: &JobDeps,
    gh: &Arc<GitHub>,
    job: &LeasedJob,
    pr: &PullRequest,
    text: Option<String>,
) -> anyhow::Result<crate::config::repo::RepoConfig> {
    let _ = deps;
    let text = match text {
        Some(t) => t,
        None => return Ok(default_repo_config()),
    };
    match crate::config::repo::RepoConfig::parse(&text) {
        Ok(c) => Ok(c),
        Err(e) => {
            let input = CheckRunInput {
                name: "barry/config".into(),
                head_sha: pr.head.sha.clone(),
                status: CheckStatus::Completed,
                conclusion: Some(CheckConclusion::Failure),
                output: CheckOutput {
                    title: "Invalid .barry.toml".into(),
                    summary: format!("{e}"),
                    text: None,
                },
            };
            let _ = gh
                .create_check_run(&job.repo_owner, &job.repo_name, &input)
                .await;
            anyhow::bail!("malformed .barry.toml; skipping other checkers")
        }
    }
}

fn default_repo_config() -> crate::config::repo::RepoConfig {
    crate::config::repo::RepoConfig::default()
}

/// The least time between two of [`watch_head`]'s lookups of the PR.
const RECHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Cancel the review when a push has moved the PR's head off `reviewed_head`.
///
/// Each lookup uses a client freshly minted by `gh_factory`, falling back to
/// `gh` when minting fails.
///
/// Woken by [`CancelRegistry::push_arrived`](crate::dispatcher::cancel::CancelRegistry::push_arrived)
/// for each push delivery whose head differs from `reviewed_head`. The
/// delivery's head is not trusted as the current one, since a redelivered or
/// late delivery carries an old head; the current head is read from GitHub.
/// A wake's lookup that does not show a moved head, including one that fails,
/// is repeated once after `interval`, in case GitHub's read lags the push.
/// Lookups are at least `interval` apart, so a stream of deliveries costs at
/// most one lookup per `interval`. A lookup that fails leaves the review
/// running, and [`still_current`] checks the head again before anything is
/// posted. Returns once the review is cancelled, by this watcher or anything
/// else.
async fn watch_head(
    gh: &Arc<GitHub>,
    gh_factory: &dyn GhFactory,
    job: &LeasedJob,
    reviewed_head: &str,
    registered: &crate::dispatcher::cancel::Registered,
    interval: Duration,
) {
    // Whether the next lookup is the repeat of one that showed no move,
    // rather than one waiting for a wake.
    let mut repeat = false;
    loop {
        if !repeat {
            tokio::select! {
                _ = registered.token.cancelled() => return,
                _ = registered.recheck.notified() => {}
            }
        }
        // The review can outlive the token `gh` was minted with, so each
        // lookup mints its own, as posting does.
        let lookup_gh = match gh_factory.for_installation(job.installation_id).await {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(
                    ?e,
                    "failed to re-mint token before re-checking the head; using original"
                );
                Arc::clone(gh)
            }
        };
        match lookup_gh
            .get_pr(&job.repo_owner, &job.repo_name, job.pr_number)
            .await
        {
            Ok(now) if now.head.sha != reviewed_head => {
                tracing::info!(
                    pr = job.pr_number,
                    owner = %job.repo_owner,
                    repo = %job.repo_name,
                    reviewed = %reviewed_head,
                    head = %now.head.sha,
                    "head moved; cancelling the review of the old head"
                );
                metrics::counter!("barry_review_superseded_total", "by" => "push").increment(1);
                registered.token.cancel();
                return;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    ?e,
                    pr = job.pr_number,
                    "could not re-check the head after a push"
                );
            }
        }
        repeat = !repeat;
        tokio::select! {
            _ = registered.token.cancelled() => return,
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

/// Whether the pull request is still the one this outcome was produced for.
///
/// A push cancels the review in flight through [`watch_head`], but the
/// delivery can lag or, through smee, never arrive; and a review that finished a moment
/// before the push still has its outcome in hand. So the head is checked once
/// more here, at the last moment before anything is written. A review of a
/// commit that is no longer the head is worth nothing, and posting it
/// misleads: the check-run lands on a stale SHA and the review comments sit
/// under lines that may no longer exist.
///
/// A lookup that fails does not block the post: losing a good review to a
/// transient GitHub error is the worse trade.
async fn still_current(gh: &Arc<GitHub>, job: &LeasedJob, pr: &PullRequest) -> bool {
    let now = match gh
        .get_pr(&job.repo_owner, &job.repo_name, job.pr_number)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                ?e,
                pr = job.pr_number,
                "could not re-check the head before posting; posting anyway"
            );
            return true;
        }
    };
    if now.head.sha != pr.head.sha {
        tracing::info!(
            pr = job.pr_number,
            owner = %job.repo_owner,
            repo = %job.repo_name,
            reviewed = %pr.head.sha,
            head = %now.head.sha,
            "head moved during the review; not posting a stale outcome"
        );
        metrics::counter!("barry_review_stale_total", "reason" => "head_moved").increment(1);
        return false;
    }
    if now.state != "open" {
        tracing::info!(
            pr = job.pr_number,
            owner = %job.repo_owner,
            repo = %job.repo_name,
            state = %now.state,
            "PR is no longer open; not posting"
        );
        metrics::counter!("barry_review_stale_total", "reason" => "closed").increment(1);
        return false;
    }
    true
}

async fn post_outcome(
    gh: &Arc<GitHub>,
    job: &LeasedJob,
    pr: &PullRequest,
    o: &CheckerOutcome,
    cancel: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    if !still_current(gh, job, pr).await {
        return Ok(());
    }
    let span = tracing::info_span!(
        "checker.post_outcome",
        checker = o.checker_name,
        pr = job.pr_number,
        owner = %job.repo_owner,
        repo = %job.repo_name,
        status = ?match o.status {
            OutcomeStatus::Success => CheckConclusion::Success,
            OutcomeStatus::Neutral => CheckConclusion::Neutral,
            OutcomeStatus::Failure => CheckConclusion::Failure,
        }
    );
    async {
        // Re-check cancellation between each remote call. Without this, a close
        // event arriving mid-`post_outcome` could leave us partially posted
        // (e.g. a check-run created but no inline review or issue comment).
        if cancel.is_cancelled() {
            tracing::info!(
                checker = o.checker_name,
                "post_outcome cancelled before check-run"
            );
            return Ok(());
        }
        let conclusion = match o.status {
            OutcomeStatus::Success => CheckConclusion::Success,
            OutcomeStatus::Neutral => CheckConclusion::Neutral,
            OutcomeStatus::Failure => CheckConclusion::Failure,
        };
        let input = CheckRunInput {
            name: o.checker_name.to_string(),
            head_sha: pr.head.sha.clone(),
            status: CheckStatus::Completed,
            conclusion: Some(conclusion),
            output: CheckOutput {
                title: o.checker_name.to_string(),
                summary: o.summary.clone(),
                text: o.text.clone(),
            },
        };
        let _ = gh
            .create_check_run(&job.repo_owner, &job.repo_name, &input)
            .await?;
        tracing::debug!(checker = o.checker_name, conclusion = ?conclusion, "check-run posted");
        if cancel.is_cancelled() {
            tracing::info!(
                checker = o.checker_name,
                "post_outcome cancelled after check-run"
            );
            return Ok(());
        }
        if !o.add_labels.is_empty() {
            gh.add_labels(
                &job.repo_owner,
                &job.repo_name,
                job.pr_number,
                &o.add_labels,
            )
            .await?;
            tracing::debug!(checker = o.checker_name, labels = ?o.add_labels, "labels added");
            if cancel.is_cancelled() {
                tracing::info!(
                    checker = o.checker_name,
                    "post_outcome cancelled after labels"
                );
                return Ok(());
            }
        }
        if !o.inline_comments.is_empty() {
            let review = ReviewInput {
                body: &o.summary,
                event: "COMMENT",
                comments: &o.inline_comments,
                commit_id: &pr.head.sha,
            };
            let _ = gh
                .create_review(&job.repo_owner, &job.repo_name, job.pr_number, &review)
                .await?;
            tracing::info!(
                inline_comments = o.inline_comments.len(),
                "pr review posted"
            );
            if cancel.is_cancelled() {
                tracing::info!(
                    checker = o.checker_name,
                    "post_outcome cancelled after review"
                );
                return Ok(());
            }
        }
        if let Some(body) = &o.issue_comment {
            let _ = gh
                .create_issue_comment(&job.repo_owner, &job.repo_name, job.pr_number, body)
                .await?;
            tracing::debug!(
                checker = o.checker_name,
                body_chars = body.len(),
                "issue comment posted"
            );
        }
        Ok(())
    }
    .instrument(span)
    .await
}

/// One line of an error, short enough for a check-run summary.
///
/// First line only: the chain below it is context for the log, not for someone
/// glancing at a pull request.
fn summarize_error(e: &anyhow::Error) -> String {
    let msg = e.to_string();
    let first = msg.lines().next().unwrap_or("internal error").trim();
    let mut out: String = first.chars().take(300).collect();
    if first.chars().count() > 300 {
        out.push('\u{2026}');
    }
    out
}

fn status_str(s: OutcomeStatus) -> &'static str {
    match s {
        OutcomeStatus::Success => "success",
        OutcomeStatus::Neutral => "neutral",
        OutcomeStatus::Failure => "failure",
    }
}

use crate::util::now_ts;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_checker_says_what_failed_rather_than_see_logs() {
        // What a reader sees on the pull request. "internal error (see logs)"
        // made "the rack was busy" and "barry is broken" identical, and the
        // logs are on a host they may not have.
        let e = anyhow::anyhow!(
            "the rack was busy: experiment 01a0 waited 900s without being given a host"
        );
        let s = summarize_error(&e);
        assert!(s.contains("the rack was busy"), "{s}");
    }

    #[test]
    fn a_long_error_is_cut_to_one_line_and_a_readable_length() {
        let e = anyhow::anyhow!("{}\nsecond line that should not appear", "x".repeat(400));
        let s = summarize_error(&e);
        assert!(!s.contains("second line"), "multi-line summary");
        assert!(s.chars().count() <= 301, "{} chars", s.chars().count());
    }

    fn rest_pr(head: &str) -> serde_json::Value {
        serde_json::json!({
            "number": 1, "title": "feat: x", "body": "ok",
            "user": { "login": "alice" }, "draft": false, "state": "open",
            "head": { "sha": head, "ref": "x" }, "base": { "sha": "base", "ref": "main" },
            "additions": 1, "deletions": 0, "changed_files": 1
        })
    }

    fn job() -> LeasedJob {
        LeasedJob {
            id: 1,
            installation_id: 1,
            repo_owner: "o".into(),
            repo_name: "r".into(),
            pr_number: 1,
            event_kind: "pull_request.synchronize".into(),
            delivery_id: "d".into(),
            attempts: 0,
            actor: None,
            rack_experiment: None,
        }
    }

    /// Mints `self.0` for every installation.
    struct SameGh(Arc<GitHub>);

    #[async_trait::async_trait]
    impl GhFactory for SameGh {
        async fn for_installation(&self, _id: i64) -> anyhow::Result<Arc<GitHub>> {
            Ok(self.0.clone())
        }
    }

    /// Fails to mint any client.
    struct NoGh;

    #[async_trait::async_trait]
    impl GhFactory for NoGh {
        async fn for_installation(&self, _id: i64) -> anyhow::Result<Arc<GitHub>> {
            anyhow::bail!("could not mint an installation token")
        }
    }

    async fn pr_lookups(server: &wiremock::MockServer) -> usize {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.method.as_str() == "GET" && r.url.path() == "/repos/o/r/pulls/1")
            .count()
    }

    #[tokio::test]
    async fn a_stream_of_wakes_costs_at_most_one_lookup_per_interval() {
        // Replayed deliveries of an old push each wake the watcher. GitHub
        // keeps saying the head is the one under review, so nothing is
        // cancelled, and the lookups are spaced by the interval rather than
        // made back to back.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(rest_pr("h1")))
            .mount(&server)
            .await;
        let gh =
            Arc::new(GitHub::new(reqwest::Client::new(), "tok".into()).with_base(server.uri()));
        let reg = crate::dispatcher::cancel::CancelRegistry::new();
        let r = reg.register("o", "r", 1, "h1").await.unwrap();
        let job = job();
        let factory = SameGh(gh.clone());
        let watcher = watch_head(&gh, &factory, &job, "h1", &r, Duration::from_millis(200));
        let wakes = async {
            loop {
                r.recheck.notify_one();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        let ran = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                _ = watcher => {}
                _ = wakes => {}
            }
        })
        .await;
        assert!(ran.is_err(), "the watcher should still be watching");
        assert!(!r.token.is_cancelled());
        let n = pr_lookups(&server).await;
        assert!(n <= 6, "{n} lookups in 1s at one per 200ms");
    }

    #[tokio::test]
    async fn a_head_github_has_not_yet_moved_is_looked_up_once_more() {
        // A push was delivered, but the first lookup still returns the head
        // under review. One more lookup after the interval sees the new head.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(rest_pr("h1")))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(rest_pr("h2")))
            .mount(&server)
            .await;
        let gh =
            Arc::new(GitHub::new(reqwest::Client::new(), "tok".into()).with_base(server.uri()));
        let reg = crate::dispatcher::cancel::CancelRegistry::new();
        let r = reg.register("o", "r", 1, "h1").await.unwrap();
        let job = job();
        r.recheck.notify_one();
        tokio::time::timeout(
            Duration::from_secs(2),
            watch_head(
                &gh,
                &SameGh(gh.clone()),
                &job,
                "h1",
                &r,
                Duration::from_millis(100),
            ),
        )
        .await
        .expect("the watcher should have looked again and cancelled");
        assert!(r.token.is_cancelled());
        assert_eq!(pr_lookups(&server).await, 2);
    }

    #[tokio::test]
    async fn a_lookup_uses_a_freshly_minted_client_not_the_one_the_job_started_with() {
        // A review can outlive the installation token it started with. GitHub
        // rejects the expired token, and accepts only the one minted for the
        // lookup, which shows the head has moved.
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls/1"))
            .and(header("Authorization", "Bearer fresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(rest_pr("h2")))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls/1"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let expired =
            Arc::new(GitHub::new(reqwest::Client::new(), "expired".into()).with_base(server.uri()));
        let fresh =
            Arc::new(GitHub::new(reqwest::Client::new(), "fresh".into()).with_base(server.uri()));
        let reg = crate::dispatcher::cancel::CancelRegistry::new();
        let r = reg.register("o", "r", 1, "h1").await.unwrap();
        let job = job();
        r.recheck.notify_one();
        tokio::time::timeout(
            Duration::from_secs(2),
            watch_head(
                &expired,
                &SameGh(fresh),
                &job,
                "h1",
                &r,
                Duration::from_millis(100),
            ),
        )
        .await
        .expect("the watcher should have seen the moved head and cancelled");
        assert!(r.token.is_cancelled());
    }

    #[tokio::test]
    async fn a_lookup_falls_back_to_the_job_client_when_minting_fails() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(rest_pr("h2")))
            .mount(&server)
            .await;
        let gh =
            Arc::new(GitHub::new(reqwest::Client::new(), "tok".into()).with_base(server.uri()));
        let reg = crate::dispatcher::cancel::CancelRegistry::new();
        let r = reg.register("o", "r", 1, "h1").await.unwrap();
        let job = job();
        r.recheck.notify_one();
        tokio::time::timeout(
            Duration::from_secs(2),
            watch_head(&gh, &NoGh, &job, "h1", &r, Duration::from_millis(100)),
        )
        .await
        .expect("the watcher should have looked up with the job's client and cancelled");
        assert!(r.token.is_cancelled());
    }
}
