use crate::checker::multi_review::identity::Identity;
use crate::checker::multi_review::review::{Outcome, UnifiedReview};
use crate::dispatcher::run::{GhFactoryError, MultiGhFactory};
use crate::github::pr::{ChangedFile, ReviewCommentInput, ReviewInput};
use std::collections::BTreeMap;
use std::sync::Arc;

pub const REVIEW_MARKER_PREFIX: &str = "<!-- barry-dylan:multi-review:";

/// The review body. `tie_break` marks a review whose outcome decided the
/// check-run because Barry and Other Barry disagreed.
pub fn body_for(
    identity: Identity,
    review: &UnifiedReview,
    peer_disagreement: Option<&str>,
    tie_break: bool,
) -> String {
    let header = format!(
        "{REVIEW_MARKER_PREFIX}{slug}:v1 -->\n**{label}** — {outcome:?}{note}\n",
        slug = identity.slug(),
        label = identity.label(),
        outcome = review.outcome,
        note = if tie_break { " (tie-break)" } else { "" },
    );
    let mut body = header;
    if let Some(disagreement) = peer_disagreement {
        body.push_str("\n> ");
        body.push_str(disagreement);
        body.push_str("\n\n");
    }
    body.push_str(&review.summary);
    body
}

#[allow(clippy::too_many_arguments)]
pub async fn post_review(
    factory: &Arc<dyn MultiGhFactory>,
    owner: &str,
    repo: &str,
    identity: Identity,
    pr_number: i64,
    head_sha: &str,
    files: &[ChangedFile],
    review: &UnifiedReview,
    peer_disagreement: Option<&str>,
    tie_break: bool,
) -> Result<(), GhFactoryError> {
    let gh = factory.for_identity(identity, owner, repo).await?;
    let inline = to_inline_comments(files, &review.findings);
    tracing::info!(
        ?identity,
        outcome = ?review.outcome,
        findings = review.findings.len(),
        inline_comments = inline.len(),
        "posting review"
    );
    let body = body_for(identity, review, peer_disagreement, tie_break);
    let input = ReviewInput {
        body: &body,
        event: review_event(review.outcome),
        comments: &inline,
        commit_id: head_sha,
    };
    gh.create_review(owner, repo, pr_number, &input)
        .await
        .map_err(|e| GhFactoryError::Other(e.into()))?;
    tracing::info!(?identity, "review posted");
    Ok(())
}

/// The GitHub review event for an outcome. The outcome comes from a model
/// reading PR content the author controls, and an App's `APPROVE` review can
/// count toward a branch's required approvals, so an Approve outcome is posted
/// as `COMMENT`. The body header still names the outcome.
fn review_event(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Approve | Outcome::Comment => "COMMENT",
        Outcome::RequestChanges => "REQUEST_CHANGES",
    }
}

fn to_inline_comments(
    files: &[ChangedFile],
    findings: &[crate::checker::multi_review::review::UnifiedFinding],
) -> Vec<ReviewCommentInput> {
    let mut by_file: BTreeMap<&str, &ChangedFile> = BTreeMap::new();
    for f in files {
        by_file.insert(f.filename.as_str(), f);
    }
    findings
        .iter()
        .filter_map(|f| {
            let cf = by_file.get(f.file.as_str())?;
            let patch = cf.patch.as_deref()?;
            let pos = patch_position_for_new_line(patch, f.line)?;
            Some(ReviewCommentInput {
                path: f.file.clone(),
                position: pos as i64,
                body: f.message.clone(),
            })
        })
        .collect()
}

fn patch_position_for_new_line(patch: &str, target_new_line: u32) -> Option<u32> {
    let mut pos = 0u32;
    let mut new_line = 0u32;
    for line in patch.lines() {
        if line.starts_with("@@") {
            if let Some(plus) = line.split_whitespace().find(|s| s.starts_with('+')) {
                let n = plus.trim_start_matches('+');
                let start: u32 = n.split(',').next()?.parse().ok()?;
                new_line = start.saturating_sub(1);
            }
            pos += 1;
            continue;
        }
        pos += 1;
        if line.starts_with('-') {
            continue;
        }
        new_line += 1;
        if new_line == target_new_line {
            return Some(pos);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rev(outcome: Outcome) -> UnifiedReview {
        UnifiedReview {
            outcome,
            summary: "looks fine".into(),
            findings: vec![],
        }
    }

    #[test]
    fn body_includes_identity_marker_and_label() {
        let b = body_for(Identity::OtherBarry, &rev(Outcome::Comment), None, false);
        assert!(b.contains("multi-review:other_barry"));
        assert!(b.contains("Other Barry"));
        assert!(b.contains("looks fine"));
    }

    #[test]
    fn disagreement_quote_appears_above_summary() {
        let b = body_for(
            Identity::OtherBarry,
            &rev(Outcome::Comment),
            Some("I disagree with Barry on X"),
            false,
        );
        let disagree_idx = b.find("disagree").unwrap();
        let summary_idx = b.find("looks fine").unwrap();
        assert!(disagree_idx < summary_idx);
    }

    /// Hands out a client pointed at a mock GitHub for every identity.
    struct MockFactory {
        base: String,
    }

    #[async_trait::async_trait]
    impl crate::dispatcher::run::GhFactory for MockFactory {
        async fn for_installation(
            &self,
            _installation_id: i64,
        ) -> anyhow::Result<Arc<crate::github::client::GitHub>> {
            anyhow::bail!("not used in this test")
        }
    }

    #[async_trait::async_trait]
    impl MultiGhFactory for MockFactory {
        async fn for_identity(
            &self,
            _identity: Identity,
            _owner: &str,
            _repo: &str,
        ) -> Result<Arc<crate::github::client::GitHub>, GhFactoryError> {
            Ok(Arc::new(
                crate::github::client::GitHub::new(reqwest::Client::new(), "t".into())
                    .with_base(self.base.clone()),
            ))
        }

        async fn preflight_identity(
            &self,
            _identity: Identity,
            _owner: &str,
            _repo: &str,
        ) -> Result<(), GhFactoryError> {
            Ok(())
        }
    }

    /// Posts `outcome` as `identity` through `post_review` and returns the
    /// JSON body GitHub received.
    async fn posted_review(identity: Identity, outcome: Outcome) -> serde_json::Value {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/acme/widget/pulls/7/reviews"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 1})))
            .expect(1)
            .mount(&server)
            .await;
        let factory: Arc<dyn MultiGhFactory> = Arc::new(MockFactory { base: server.uri() });
        post_review(
            &factory,
            "acme",
            "widget",
            identity,
            7,
            "deadbeef",
            &[],
            &rev(outcome),
            None,
            false,
        )
        .await
        .unwrap();
        let requests = server.received_requests().await.unwrap();
        serde_json::from_slice(&requests[0].body).unwrap()
    }

    #[test]
    fn no_outcome_maps_to_an_approve_event() {
        assert_eq!(review_event(Outcome::Approve), "COMMENT");
        assert_eq!(review_event(Outcome::Comment), "COMMENT");
        assert_eq!(review_event(Outcome::RequestChanges), "REQUEST_CHANGES");
    }

    #[tokio::test]
    async fn an_approve_outcome_is_posted_as_a_comment_review_under_every_identity() {
        for identity in [
            Identity::Barry,
            Identity::OtherBarry,
            Identity::OtherOtherBarry,
        ] {
            let body = posted_review(identity, Outcome::Approve).await;
            assert_eq!(body["event"], "COMMENT", "{identity:?}");
            let header = format!("**{}** — Approve", identity.label());
            assert!(
                body["body"].as_str().unwrap().contains(&header),
                "{identity:?}: {body}"
            );
        }
    }

    #[tokio::test]
    async fn a_request_changes_outcome_is_posted_as_request_changes() {
        let body = posted_review(Identity::Barry, Outcome::RequestChanges).await;
        assert_eq!(body["event"], "REQUEST_CHANGES");
    }

    #[test]
    fn position_for_added_line() {
        let patch = "@@ -1,2 +1,3 @@\n a\n+b\n c\n";
        assert_eq!(patch_position_for_new_line(patch, 2), Some(3));
    }
}
