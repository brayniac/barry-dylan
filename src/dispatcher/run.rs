use crate::checker::{Checker, CheckerCtx, CheckerOutcome, OutcomeStatus};
use crate::config::Config;
use crate::dispatcher::trust::{self, BarryCommand, Trust};
use crate::github::check_run::{CheckConclusion, CheckOutput, CheckRunInput, CheckStatus};
use crate::github::client::GitHub;
use crate::github::pr::{BotComment, ChangedFile, PullRequest, ReviewCommentInput, ReviewInput};
use crate::storage::queue::LeasedJob;
use crate::storage::Store;
use std::sync::Arc;
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
    pub gh_factory: Arc<dyn GhFactory>,
}

#[async_trait::async_trait]
pub trait GhFactory: Send + Sync {
    async fn for_installation(&self, installation_id: i64) -> anyhow::Result<Arc<GitHub>>;
}

pub async fn run_job(deps: &JobDeps, job: &LeasedJob) -> anyhow::Result<()> {
    let gh = deps.gh_factory.for_installation(job.installation_id).await?;

    if let Some(cmd) = parse_command_event(&job.event_kind) {
        return handle_command(deps, &gh, job, cmd).await;
    }

    let pr = gh.get_pr(&job.repo_owner, &job.repo_name, job.pr_number).await?;
    let files = gh.list_pr_files(&job.repo_owner, &job.repo_name, job.pr_number).await?;
    let perm = gh.author_permission(&job.repo_owner, &job.repo_name, &pr.user.login).await
        .unwrap_or_else(|_| "read".into());
    let prior_comments = gh.list_pr_comments(&job.repo_owner, &job.repo_name, job.pr_number).await
        .unwrap_or_default();
    let prior_reviews = gh.list_pr_reviews(&job.repo_owner, &job.repo_name, job.pr_number).await
        .unwrap_or_default();
    let bot_comments: Vec<BotComment> = prior_comments.iter()
        .filter(|c| c.author.starts_with("barry-bot")).cloned().collect();
    let bot_reviews: Vec<BotComment> = prior_reviews.iter()
        .filter(|c| c.author.starts_with("barry-bot")).cloned().collect();

    // Trust gate.
    let trust_decision = trust::evaluate_trust(&perm, &bot_comments);
    if trust_decision == Trust::NeedsApproval {
        post_needs_approval_once(&gh, job, &pr, &bot_comments).await?;
        return Ok(());
    }

    // Repo config.
    let default_branch = pr.base.r#ref.clone(); // PR's base ref ≈ default branch for most PRs
    let cfg_text = gh.get_repo_config_text(&job.repo_owner, &job.repo_name, &default_branch).await
        .unwrap_or(None);
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
    };

    let mut tasks = Vec::new();
    for chk in &deps.pipeline.checkers {
        if !chk.enabled(&ctx.repo_cfg) { continue; }
        let chk = chk.clone();
        let ctx_ref = &ctx;
        tasks.push(async move {
            let t = std::time::Instant::now();
            let res = tokio::time::timeout(Duration::from_secs(60), chk.run(ctx_ref)).await;
            (chk.name(), res, t.elapsed())
        });
    }
    let results = futures::future::join_all(tasks).await;

    // Post outcomes.
    for (name, res, dur) in results {
        let outcome = match res {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                tracing::error!(checker = name, error = ?e, "checker failed");
                CheckerOutcome::neutral(static_name(name), format!("internal error: {e}"))
            }
            Err(_) => CheckerOutcome::neutral(static_name(name), "timed out"),
        };
        post_outcome(&gh, job, &pr, &outcome).await?;
        let _ = deps.store.append_audit(&crate::storage::audit::AuditEntry {
            ts: now_ts(),
            delivery_id: Some(&job.delivery_id),
            repo_owner: Some(&job.repo_owner),
            repo_name: Some(&job.repo_name),
            pr_number: Some(job.pr_number),
            checker_name: Some(name),
            outcome: status_str(outcome.status),
            duration_ms: Some(dur.as_millis() as i64),
            details: None,
        }).await;
    }
    Ok(())
}

fn parse_command_event(kind: &str) -> Option<BarryCommand> {
    let sub = kind.strip_prefix("issue_comment.")?;
    Some(match sub {
        "approve" => BarryCommand::Approve,
        "review" => BarryCommand::Review,
        _ => BarryCommand::Unknown,
    })
}

async fn handle_command(
    deps: &JobDeps, gh: &Arc<GitHub>, job: &LeasedJob, cmd: BarryCommand,
) -> anyhow::Result<()> {
    match cmd {
        BarryCommand::Approve => {
            gh.create_issue_comment(&job.repo_owner, &job.repo_name, job.pr_number,
                &trust::approve_comment_body()).await?;
            // Re-enqueue a normal review job for this PR.
            let now = now_ts();
            deps.store.enqueue(&crate::storage::queue::NewJob {
                installation_id: job.installation_id,
                repo_owner: job.repo_owner.clone(),
                repo_name: job.repo_name.clone(),
                pr_number: job.pr_number,
                event_kind: "pull_request.opened".into(),
                delivery_id: job.delivery_id.clone(),
            }, now, now).await?;
        }
        BarryCommand::Review => {
            let now = now_ts();
            deps.store.enqueue(&crate::storage::queue::NewJob {
                installation_id: job.installation_id,
                repo_owner: job.repo_owner.clone(),
                repo_name: job.repo_name.clone(),
                pr_number: job.pr_number,
                event_kind: "pull_request.opened".into(),
                delivery_id: job.delivery_id.clone(),
            }, now, now).await?;
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
    gh: &Arc<GitHub>, job: &LeasedJob, pr: &PullRequest, bot_comments: &[BotComment],
) -> anyhow::Result<()> {
    if bot_comments.iter().any(|c| c.body.contains(trust::NEEDS_APPROVAL_MARKER)) {
        return Ok(());
    }
    gh.create_issue_comment(
        &job.repo_owner, &job.repo_name, job.pr_number,
        &trust::needs_approval_body(&pr.user.login),
    ).await?;
    Ok(())
}

async fn parse_repo_config_with_check(
    deps: &JobDeps, gh: &Arc<GitHub>, job: &LeasedJob, pr: &PullRequest, text: Option<String>,
) -> anyhow::Result<crate::config::repo::RepoConfig> {
    let _ = deps;
    let text = match text { Some(t) => t, None => return Ok(default_repo_config()) };
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
            let _ = gh.create_check_run(&job.repo_owner, &job.repo_name, &input).await;
            anyhow::bail!("malformed .barry.toml; skipping other checkers")
        }
    }
}

fn default_repo_config() -> crate::config::repo::RepoConfig {
    crate::config::repo::RepoConfig::default()
}

async fn post_outcome(
    gh: &Arc<GitHub>, job: &LeasedJob, pr: &PullRequest, o: &CheckerOutcome,
) -> anyhow::Result<()> {
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
    let _ = gh.create_check_run(&job.repo_owner, &job.repo_name, &input).await?;
    if !o.add_labels.is_empty() {
        let _ = gh.add_labels(&job.repo_owner, &job.repo_name, job.pr_number, &o.add_labels).await?;
    }
    if !o.inline_comments.is_empty() {
        // Minimize prior LLM review (Task 33).
        let review = ReviewInput {
            body: &o.summary,
            event: "COMMENT",
            comments: &o.inline_comments,
            commit_id: &pr.head.sha,
        };
        let _ = gh.create_review(&job.repo_owner, &job.repo_name, job.pr_number, &review).await?;
    }
    if let Some(body) = &o.issue_comment {
        let _ = gh.create_issue_comment(&job.repo_owner, &job.repo_name, job.pr_number, body).await?;
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
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}
