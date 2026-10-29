//! Confer command handler. Summons OB or OOB to weigh in on a posted review.

use crate::checker::multi_review::clients::IdentityClients;
use crate::checker::multi_review::identity::Identity;
use crate::checker::multi_review::orchestrator::run_personas_gated;
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

    // Authorize against the user who actually issued `/barry confer`, threaded
    // through from the webhook. Older queued jobs may lack this field (pre-
    // migration); in that case we conservatively reject rather than fall back
    // to a guess at the commenter.
    let Some(confer_author) = job.actor.clone() else {
        tracing::warn!("confer job missing actor field; ignoring");
        metrics::counter!("barry_confer_total", "outcome" => "rejected_unauthorized").increment(1);
        return Ok(());
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

    // Preflight: verify the summoned identity is installed before spending LLM tokens.
    let owner = job.repo_owner.clone();
    let repo = job.repo_name.clone();
    match deps
        .gh_factory
        .preflight_identity(summon, &owner, &repo)
        .await
    {
        Ok(()) => {}
        Err(crate::dispatcher::run::GhFactoryError::NotInstalled { identity, .. }) => {
            let identity_label = identity.label();
            let body = format!(
                "{identity_label} isn't installed on this repository, so I can't summon them. \
                 Ask a maintainer to install the App."
            );
            barry_gh
                .create_issue_comment(&owner, &repo, job.pr_number, &body)
                .await?;
            metrics::counter!(
                "barry_confer_total",
                "outcome" => "rejected_not_installed"
            )
            .increment(1);
            return Ok(());
        }
        Err(crate::dispatcher::run::GhFactoryError::Other(e)) => {
            return Err(e);
        }
    }

    let files = barry_gh.list_pr_files(&owner, &repo, job.pr_number).await?;
    let name = format!("barry confer {owner}/{repo} #{}", job.pr_number);
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let (review, tokens) = review_as(
        deps.config.rack.as_ref(),
        &http,
        clients,
        summon,
        personas,
        &files,
        &name,
    )
    .await?;
    deps.status_tracker
        .add_tokens(job.id, tokens.input, tokens.output);

    post_review(
        &deps.gh_factory,
        &owner,
        &repo,
        summon,
        job.pr_number,
        &head_sha,
        &files,
        &review,
        None,
        false,
    )
    .await
    .map_err(|e| anyhow::anyhow!(e))?;

    let now = crate::util::now_ts();
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

/// Why [`run_unified`] produced no review.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ReviewError {
    #[error("persona call failed: {0}")]
    Persona(crate::llm::LlmError),
    #[error("synthesis failed: {0}")]
    Synthesis(synthesis::SynthesisError),
    #[error("rack review failed: {0}")]
    Rack(crate::rack::RackError),
    #[error("the rack job returned no review for {0}")]
    RackMissing(&'static str),
}

impl ReviewError {
    /// A fixed description for text that is published, such as a check-run
    /// summary. The error itself can carry an endpoint URL or a response body
    /// of any size; it belongs in the log.
    pub(crate) fn category(&self) -> &'static str {
        match self {
            ReviewError::Persona(_) | ReviewError::Synthesis(synthesis::SynthesisError::Llm(_)) => {
                "model request failed"
            }
            ReviewError::Synthesis(_) => "synthesis failed",
            ReviewError::Rack(_) | ReviewError::RackMissing(_) => "rack review failed",
        }
    }
}

/// `identity`'s independent review of `files`: in a rack job of its own when
/// `[rack]` lists a reviewer for it, so the review gets a freshly started
/// model server, and otherwise against the identity's endpoint
/// ([`run_unified`]).
///
/// The rack job is not remembered on the dispatcher job, as the main rack
/// review is: dropped before it ends, it is cancelled, and a resumed job runs
/// it again. It runs one priority above a main review. A rack review reports
/// no token counts.
pub(crate) async fn review_as(
    rack: Option<&crate::rack::RackConfig>,
    http: &reqwest::Client,
    clients: &IdentityClients,
    identity: Identity,
    personas: &[Persona],
    files: &[crate::github::pr::ChangedFile],
    name: &str,
) -> Result<(UnifiedReview, synthesis::TokenCount), ReviewError> {
    let slug = identity.slug();
    if let Some(rack) = rack.filter(|r| r.has_reviewer(slug)) {
        let mut cfg = rack.clone();
        cfg.reviewers.retain(|r| r.identity == slug);
        cfg.judge = false;
        // One above a main review: a tie-break follows a main review that has
        // finished, and should not queue behind other pull requests' ones.
        cfg.priority = cfg.priority.saturating_add(1);
        let mut outcome = crate::rack::review(&cfg, http, files, name)
            .await
            .map_err(ReviewError::Rack)?;
        let review = outcome
            .reviews
            .remove(slug)
            .ok_or(ReviewError::RackMissing(slug))?;
        return Ok((review, synthesis::TokenCount::default()));
    }
    let diff = synthesis::render_diff_block(files);
    run_unified(clients, identity, personas, &diff).await
}

/// One reviewer's independent review of `diff`: every persona drafts, gated
/// to fit the identity's context window, then one synthesis with no peer
/// review in view. Used by `/barry confer` and by the tie-break that follows
/// a disagreement, so a summoned reviewer and a tie-breaking one read the
/// same thing: the diff, and not the reviews already posted.
pub(crate) async fn run_unified(
    clients: &IdentityClients,
    identity: Identity,
    personas: &[Persona],
    diff: &str,
) -> Result<(UnifiedReview, synthesis::TokenCount), ReviewError> {
    let client = clients.for_identity(identity).as_ref();
    let max_tokens = clients.max_tokens_for(identity);
    let prepared = personas
        .iter()
        .map(|p| (p.clone(), diff.to_string()))
        .collect();
    let results = run_personas_gated(
        client,
        max_tokens,
        clients.context_size_for(identity),
        prepared,
    )
    .await;
    let mut drafts = Vec::with_capacity(results.len());
    let mut total = synthesis::TokenCount::default();
    for r in results {
        let draft = r.map_err(ReviewError::Persona)?;
        total.input += draft.tokens.input;
        total.output += draft.tokens.output;
        drafts.push(draft);
    }
    let (r, synth_tokens) = synthesis::synthesize(client, &drafts, diff, None, max_tokens)
        .await
        .map_err(ReviewError::Synthesis)?;
    total.input += synth_tokens.input;
    total.output += synth_tokens.output;
    Ok((r, total))
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

    /// Records the most requests it has had in flight at once.
    struct PeakClient {
        in_flight: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::llm::LlmClient for PeakClient {
        async fn complete(
            &self,
            _req: &crate::llm::LlmRequest,
        ) -> Result<crate::llm::LlmResponse, crate::llm::LlmError> {
            use std::sync::atomic::Ordering::SeqCst;
            let now = self.in_flight.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(now, SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            self.in_flight.fetch_sub(1, SeqCst);
            Ok(crate::llm::LlmResponse {
                text: r#"{"outcome":"approve","summary":"ok","findings":[]}"#.into(),
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn the_confer_review_runs_personas_in_turns_when_the_window_is_small() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let peak = Arc::new(PeakClient {
            in_flight: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });
        let client: Arc<dyn crate::llm::LlmClient> = peak.clone();
        // Four personas at ~1000 tokens each into a 2500-token window: two at
        // a time, as the primary reviewers would run them.
        let clients = IdentityClients {
            barry: client.clone(),
            other_barry: client.clone(),
            other_other_barry: client.clone(),
            judge: client,
            barry_max_tokens: 1000,
            other_barry_max_tokens: 1000,
            other_other_barry_max_tokens: 1000,
            judge_max_tokens: 1000,
            barry_context_size: None,
            other_barry_context_size: None,
            other_other_barry_context_size: Some(2500),
        };
        let personas: Vec<Persona> = ["a", "b", "c", "d"]
            .into_iter()
            .map(|n| Persona {
                name: n,
                prompt: Arc::new(format!("you are {n}")),
            })
            .collect();
        run_unified(&clients, Identity::OtherOtherBarry, &personas, "+x")
            .await
            .unwrap();
        assert_eq!(peak.peak.load(SeqCst), 2);
    }

    fn peak_clients() -> IdentityClients {
        let client: Arc<dyn crate::llm::LlmClient> = Arc::new(PeakClient {
            in_flight: std::sync::atomic::AtomicUsize::new(0),
            peak: std::sync::atomic::AtomicUsize::new(0),
        });
        IdentityClients {
            barry: client.clone(),
            other_barry: client.clone(),
            other_other_barry: client.clone(),
            judge: client,
            barry_max_tokens: 1000,
            other_barry_max_tokens: 1000,
            other_other_barry_max_tokens: 1000,
            judge_max_tokens: 1000,
            barry_context_size: None,
            other_barry_context_size: None,
            other_other_barry_context_size: None,
        }
    }

    fn rack_with_oob(systemslab: &str) -> crate::rack::RackConfig {
        toml::from_str(&format!(
            r#"
systemslab = "{systemslab}"
host_tags = ["host:hv03"]
bare = true
judge = true
packages = []
poll_interval_secs = 1

[[reviewers]]
identity = "barry"
model = "/m.gguf"
model_name = "m"

[[reviewers]]
identity = "other_barry"
model = "/m.gguf"
model_name = "m"

[[reviewers]]
identity = "other_other_barry"
model = "/m.gguf"
model_name = "m"
"#
        ))
        .unwrap()
    }

    /// A tie-break or `/barry confer` with a rack reviewer for the identity is
    /// one rack job with that reviewer alone, no judge, and never the endpoint.
    #[tokio::test]
    async fn a_summoned_rack_reviewer_runs_in_a_job_of_its_own() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/submit"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "t1"})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/experiment/t1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"state": "success", "jobs": [{"state": "complete"}]}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/experiment/t1/artifacts"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "a1", "name": "review-other_other_barry.json"}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/artifact/a1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"{"outcome":"request_changes","summary":"no","findings":[]}"#,
                ),
            )
            .mount(&server)
            .await;
        let rack = rack_with_oob(&server.uri());
        let files = vec![crate::github::pr::ChangedFile {
            filename: "a.rs".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 0,
            changes: 1,
            patch: Some("@@ -1 +1 @@\n+x".into()),
        }];
        let (review, _) = review_as(
            Some(&rack),
            &reqwest::Client::new(),
            &peak_clients(),
            Identity::OtherOtherBarry,
            &[],
            &files,
            "t",
        )
        .await
        .unwrap();
        assert_eq!(review.summary, "no");

        let submitted = server.received_requests().await.unwrap();
        let body = String::from_utf8_lossy(&submitted[0].body).to_string();
        assert_eq!(body.matches("review-offline").count(), 1, "{body}");
        assert!(body.contains("offline-other_other_barry.toml"), "{body}");
        assert!(!body.contains("offline-barry.toml"), "{body}");
        assert!(!body.contains("judge-offline"), "{body}");
        // One above a main review's 300.
        assert!(body.contains("\"priority\":301"), "{body}");
    }

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
