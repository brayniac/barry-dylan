use crate::checker::multi_review::identity::Identity;
use crate::checker::{Checker, CheckerCtx, CheckerOutcome, OutcomeStatus};
use crate::config::Config;
use crate::dispatcher::trust::{self, BarryCommand, Trust};
use crate::github::check_run::{CheckConclusion, CheckOutput, CheckRunInput, CheckStatus};
use crate::github::client::{GhError, GitHub};
use crate::github::pr::{BotComment, PullRequest, ReviewInput};
use crate::storage::Store;
use crate::storage::queue::LeasedJob;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
}

#[async_trait::async_trait]
pub trait GhFactory: Send + Sync {
    async fn for_installation(&self, installation_id: i64) -> anyhow::Result<Arc<GitHub>>;
}

#[async_trait::async_trait]
pub trait MultiGhFactory: GhFactory {
    /// Mint a GitHub client authenticated as the given identity for this installation.
    async fn for_identity(
        &self,
        identity: Identity,
        installation_id: i64,
    ) -> anyhow::Result<Arc<GitHub>>;
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
        pr: pr.clone(),
        files,
        prior_bot_reviews: bot_reviews,
        prior_bot_comments: bot_comments,
        store: deps.store.clone(),
        installation_id: Some(job.installation_id),
    };

    let checker_timeout = Duration::from_secs(deps.config.dispatcher.checker_timeout_secs);
    let store = &deps.store;
    let gh_ref = &gh;
    let job_ref = job;
    let pr_ref = &pr;
    let ctx_ref = &ctx;
    let rate_limit: Arc<Mutex<Option<i64>>> = Arc::new(Mutex::new(None));
    let mut tasks = Vec::new();
    for chk in &deps.pipeline.checkers {
        if !chk.enabled(&ctx.repo_cfg) {
            continue;
        }
        let chk = chk.clone();
        let rate_limit = rate_limit.clone();
        let checker_name = chk.name().to_string();
        tasks.push(async move {
            let span = tracing::info_span!(
                "checker.run",
                checker = checker_name,
                pr = job_ref.pr_number,
                owner = %job_ref.repo_owner,
                repo = %job_ref.repo_name
            );
            let _enter = span.enter();

            tracing::debug!("checker starting");
            let t = std::time::Instant::now();
            let res = tokio::time::timeout(checker_timeout, chk.run(ctx_ref)).await;
            let dur = t.elapsed();
            let outcome = match res {
                Ok(Ok(o)) => o,
                Ok(Err(e)) => {
                    tracing::error!(error = ?e, "checker failed");
                    CheckerOutcome::neutral(static_name(&checker_name), "internal error (see logs)")
                }
                Err(_) => {
                    let timeout_msg = format!("timed out after {}s", checker_timeout.as_secs());
                    tracing::warn!(timeout_msg, "checker timed out");
                    CheckerOutcome::neutral(static_name(&checker_name), &timeout_msg)
                }
            };
            tracing::info!(
                checker = checker_name,
                status = status_str(outcome.status),
                duration_ms = dur.as_millis() as u64,
                "checker completed"
            );
            if let Err(e) = post_outcome(gh_ref, job_ref, pr_ref, &outcome).await {
                if let Some(GhError::RateLimited { reset_in_secs }) = e.downcast_ref::<GhError>() {
                    let mut g = rate_limit.lock().unwrap();
                    *g = Some(g.map_or(*reset_in_secs, |c| c.max(*reset_in_secs)));
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
                    checker_name: Some(checker_name),
                    outcome: status_str(outcome.status).to_string(),
                    duration_ms: Some(dur.as_millis() as i64),
                    details: None,
                })
                .await;
        });
    }
    futures::future::join_all(tasks).await;
    if let Some(reset_in_secs) = *rate_limit.lock().unwrap() {
        return Err(GhError::RateLimited { reset_in_secs }.into());
    }
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
            tracing::info!("approving PR for review");
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

async fn post_outcome(
    gh: &Arc<GitHub>,
    job: &LeasedJob,
    pr: &PullRequest,
    o: &CheckerOutcome,
) -> anyhow::Result<()> {
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
    let _enter = span.enter();

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
            title: o.checker_name.into(),
            summary: o.summary.clone(),
            text: o.text.clone(),
        },
    };
    let _ = gh
        .create_check_run(&job.repo_owner, &job.repo_name, &input)
        .await?;
    tracing::debug!(checker = o.checker_name, conclusion = ?conclusion, "check-run posted");
    if !o.add_labels.is_empty() {
        gh.add_labels(
            &job.repo_owner,
            &job.repo_name,
            job.pr_number,
            &o.add_labels,
        )
        .await?;
        tracing::debug!(checker = o.checker_name, labels = ?o.add_labels, "labels added");
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
    }
    if let Some(body) = &o.issue_comment {
        let _ = gh
            .create_issue_comment(&job.repo_owner, &job.repo_name, job.pr_number, body)
            .await?;
        tracing::debug!(checker = o.checker_name, body_chars = body.len(), "issue comment posted");
    }
    Ok(())
}

fn static_name(s: &str) -> &'static str {
    // Leak once for &'static — only happens on the error path; bounded by checker count.
    Box::leak(s.to_string().into_boxed_str())
}

fn status_str(s: OutcomeStatus) -> &'static str {
    match s {
        OutcomeStatus::Success => "success",
        OutcomeStatus::Neutral => "neutral",
        OutcomeStatus::Failure => "failure",
    }
}

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
