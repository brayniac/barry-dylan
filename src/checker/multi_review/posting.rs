use crate::checker::multi_review::identity::Identity;
use crate::checker::multi_review::review::UnifiedReview;
use crate::dispatcher::run::MultiGhFactory;
use crate::github::pr::{ChangedFile, ReviewCommentInput, ReviewInput};
use std::collections::BTreeMap;
use std::sync::Arc;

pub const REVIEW_MARKER_PREFIX: &str = "<!-- barry-dylan:multi-review:";

pub fn body_for(
    identity: Identity,
    review: &UnifiedReview,
    peer_disagreement: Option<&str>,
) -> String {
    let header = format!(
        "{REVIEW_MARKER_PREFIX}{slug}:v1 -->\n**{label}** — {outcome:?}\n",
        slug = identity.slug(),
        label = identity.label(),
        outcome = review.outcome,
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
    installation_id: i64,
    identity: Identity,
    owner: &str,
    repo: &str,
    pr_number: i64,
    head_sha: &str,
    files: &[ChangedFile],
    review: &UnifiedReview,
    peer_disagreement: Option<&str>,
) -> anyhow::Result<()> {
    let gh = factory.for_identity(identity, installation_id).await?;
    let inline = to_inline_comments(files, &review.findings);
    let body = body_for(identity, review, peer_disagreement);
    let event = match review.outcome {
        crate::checker::multi_review::review::Outcome::Approve => "APPROVE",
        crate::checker::multi_review::review::Outcome::Comment => "COMMENT",
        crate::checker::multi_review::review::Outcome::RequestChanges => "REQUEST_CHANGES",
    };
    let input = ReviewInput {
        body: &body,
        event,
        comments: &inline,
        commit_id: head_sha,
    };
    let _ = gh.create_review(owner, repo, pr_number, &input).await?;
    Ok(())
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
    use crate::checker::multi_review::review::{Outcome, UnifiedReview};

    fn rev(outcome: Outcome) -> UnifiedReview {
        UnifiedReview {
            outcome,
            summary: "looks fine".into(),
            findings: vec![],
        }
    }

    #[test]
    fn body_includes_identity_marker_and_label() {
        let b = body_for(Identity::OtherBarry, &rev(Outcome::Comment), None);
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
        );
        let disagree_idx = b.find("disagree").unwrap();
        let summary_idx = b.find("looks fine").unwrap();
        assert!(disagree_idx < summary_idx);
    }

    #[test]
    fn position_for_added_line() {
        let patch = "@@ -1,2 +1,3 @@\n a\n+b\n c\n";
        assert_eq!(patch_position_for_new_line(patch, 2), Some(3));
    }
}
