//! Confer command handler. Summons OB or OOB to weigh in on a posted review.

use crate::checker::multi_review::identity::Identity;
use crate::checker::multi_review::persona::Persona;
use crate::checker::multi_review::posting::post_review;
use crate::checker::multi_review::review::UnifiedReview;
use crate::checker::multi_review::synthesis;
use crate::dispatcher::run::JobDeps;
use crate::github::client::GitHub;
use crate::storage::queue::LeasedJob;
use std::sync::Arc;

pub async fn handle(deps: &JobDeps, barry_gh: &Arc<GitHub>, job: &LeasedJob) -> anyhow::Result<()> {
    let (Some(clients), Some(personas)) = (deps.clients.as_ref(), deps.personas.as_ref()) else {
        tracing::info!("confer received but multi-review not configured; ignoring");
        return Ok(());
    };

    let pr_ctx = barry_gh
        .fetch_pr_context(&job.repo_owner, &job.repo_name, job.pr_number)
        .await?;
    let head_sha = pr_ctx.pr.head.sha.clone();

    // Authorize the commenter using the same permission lookup as the trust gate.
    // The webhook does not yet thread the comment author into the job; for v1 we
    // use the latest non-bot comment on the PR as a proxy.
    let confer_author = match pr_ctx
        .comments
        .iter()
        .rev()
        .find(|c| !c.author.starts_with("barry-dylan"))
    {
        Some(c) => c.author.clone(),
        None => {
            tracing::warn!("confer received but no non-bot comment on PR; ignoring");
            return Ok(());
        }
    };
    let perm = barry_gh
        .author_permission(&job.repo_owner, &job.repo_name, &confer_author)
        .await
        .unwrap_or_else(|_| "read".into());

    let allowed = &deps.config.confer.allowed;
    if !role_matches(&perm, &confer_author, &pr_ctx.pr.user.login, allowed) {
        tracing::info!(%confer_author, %perm, "confer rejected (unauthorized)");
        metrics::counter!("barry_confer_total", "outcome" => "rejected_unauthorized").increment(1);
        return Ok(());
    }

    let key = crate::storage::multi_review::RunKey {
        owner: job.repo_owner.clone(),
        repo: job.repo_name.clone(),
        pr: job.pr_number,
        head_sha: head_sha.clone(),
    };
    let st = match deps.store.run_state(key.clone()).await? {
        Some(s) => s,
        None => {
            tracing::info!(%head_sha, "confer with no prior run; replying");
            metrics::counter!("barry_confer_total", "outcome" => "rejected_no_run").increment(1);
            barry_gh
                .create_issue_comment(
                    &job.repo_owner,
                    &job.repo_name,
                    job.pr_number,
                    "I haven't reviewed this commit yet — comment again after the next review run.",
                )
                .await?;
            return Ok(());
        }
    };

    if st.confers_used >= deps.config.confer.max_per_pr {
        metrics::counter!("barry_confer_total", "outcome" => "rejected_max_reached").increment(1);
        barry_gh
            .create_issue_comment(
                &job.repo_owner,
                &job.repo_name,
                job.pr_number,
                "Maximum confers reached for this PR.",
            )
            .await?;
        return Ok(());
    }

    let summon = if !st.other_barry_posted {
        Identity::OtherBarry
    } else if !st.other_other_barry_posted {
        Identity::OtherOtherBarry
    } else {
        metrics::counter!("barry_confer_total", "outcome" => "rejected_all_posted").increment(1);
        barry_gh
            .create_issue_comment(
                &job.repo_owner,
                &job.repo_name,
                job.pr_number,
                "All Barrys have already conferred on this commit.",
            )
            .await?;
        return Ok(());
    };

    let files = barry_gh
        .list_pr_files(&job.repo_owner, &job.repo_name, job.pr_number)
        .await?;
    let diff = synthesis::render_diff_block(&files);
    let prior_text = build_prior_context(&pr_ctx);

    let review = run_unified(
        clients.for_identity(summon).as_ref(),
        clients.max_tokens_for(summon),
        personas,
        &diff,
        Some(&prior_text),
    )
    .await?;

    post_review(
        &deps.gh_factory,
        job.installation_id,
        summon,
        &job.repo_owner,
        &job.repo_name,
        job.pr_number,
        &head_sha,
        &files,
        &review,
        None,
    )
    .await?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    deps.store
        .record_post(key.clone(), summon, outcome_str(review.outcome), now)
        .await?;
    deps.store.record_confer_used(key, now).await?;
    let label = match summon {
        Identity::OtherBarry => "ob",
        Identity::OtherOtherBarry => "oob",
        Identity::Barry => "barry",
    };
    metrics::counter!("barry_confer_total", "outcome" => label).increment(1);
    Ok(())
}

fn role_matches(perm: &str, author: &str, pr_author: &str, allowed: &[String]) -> bool {
    for role in allowed {
        match role.as_str() {
            "author" if author == pr_author => return true,
            r if r == perm => return true,
            _ => {}
        }
    }
    false
}

async fn run_unified(
    client: &dyn crate::llm::LlmClient,
    max_tokens: u32,
    personas: &[Persona],
    diff: &str,
    peer: Option<&str>,
) -> anyhow::Result<UnifiedReview> {
    let mut futures = Vec::with_capacity(personas.len());
    for p in personas {
        let p = p.clone();
        let diff = diff.to_string();
        futures.push(async move { synthesis::run_persona(client, &p, &diff, max_tokens).await });
    }
    let results = futures::future::join_all(futures).await;
    let mut drafts = Vec::with_capacity(results.len());
    for r in results {
        drafts.push(r.map_err(|e| anyhow::anyhow!("persona call failed: {e}"))?);
    }
    let (r, _tokens) = synthesis::synthesize(client, &drafts, diff, peer, max_tokens)
        .await
        .map_err(|e| anyhow::anyhow!("synthesis failed: {e}"))?;
    Ok(r)
}

fn build_prior_context(pr_ctx: &crate::github::pr::PrContext) -> String {
    let mut s = String::from("=== prior reviews on this commit ===\n");
    for r in &pr_ctx.reviews {
        if r.body
            .contains(crate::checker::multi_review::posting::REVIEW_MARKER_PREFIX)
        {
            s.push_str(&r.body);
            s.push_str("\n---\n");
        }
    }
    s
}

fn outcome_str(o: crate::checker::multi_review::review::Outcome) -> &'static str {
    use crate::checker::multi_review::review::Outcome::*;
    match o {
        Approve => "approve",
        Comment => "comment",
        RequestChanges => "request_changes",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_matches_pr_author() {
        let allowed = vec!["author".to_string()];
        assert!(role_matches("read", "alice", "alice", &allowed));
        assert!(!role_matches("read", "bob", "alice", &allowed));
    }

    #[test]
    fn role_matches_write_perm() {
        let allowed = vec!["write".to_string(), "admin".to_string()];
        assert!(role_matches("write", "bob", "alice", &allowed));
        assert!(role_matches("admin", "bob", "alice", &allowed));
        assert!(!role_matches("read", "bob", "alice", &allowed));
    }

    #[test]
    fn role_matches_rejects_when_no_match() {
        let allowed = vec!["maintain".to_string()];
        assert!(!role_matches("write", "bob", "alice", &allowed));
    }
}
