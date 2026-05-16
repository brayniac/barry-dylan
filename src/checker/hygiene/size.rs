use crate::checker::{Checker, CheckerCtx, CheckerOutcome, OutcomeStatus};
use crate::config::repo::RepoConfig;
use async_trait::async_trait;

pub struct SizeChecker;

#[async_trait]
impl Checker for SizeChecker {
    fn name(&self) -> &'static str {
        "barry/hygiene.size"
    }
    fn enabled(&self, cfg: &RepoConfig) -> bool {
        cfg.hygiene.size.enabled
    }
    async fn run(&self, ctx: &CheckerCtx) -> anyhow::Result<CheckerOutcome> {
        let rule = &ctx.repo_cfg.hygiene.size;
        let total_lines = (ctx.pr.additions + ctx.pr.deletions) as u32;
        let total_files = ctx.pr.changed_files as u32;
        let over_lines = total_lines > rule.warn_lines;
        let over_files = total_files > rule.warn_files;
        if over_lines || over_files {
            let summary = format!(
                "large PR: {} lines / {} files (warn at {} / {})",
                total_lines, total_files, rule.warn_lines, rule.warn_files
            );
            Ok(CheckerOutcome {
                status: OutcomeStatus::Neutral,
                summary,
                ..CheckerOutcome::neutral(self.name(), "")
            })
        } else {
            Ok(CheckerOutcome::success(
                self.name(),
                format!("{} lines / {} files", total_lines, total_files),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::repo::SizeRule;
    use crate::github::client::GitHub;
    use crate::github::pr::{GitRef, PullRequest, User};
    use std::sync::Arc;

    fn pr(add: i64, del: i64, files: i64) -> PullRequest {
        PullRequest {
            number: 1,
            title: "t".into(),
            body: None,
            user: User { login: "a".into() },
            draft: false,
            state: "open".into(),
            head: GitRef {
                sha: "s".into(),
                r#ref: "x".into(),
            },
            base: GitRef {
                sha: "s2".into(),
                r#ref: "main".into(),
            },
            additions: add,
            deletions: del,
            changed_files: files,
        }
    }

    async fn ctx_for(pr: PullRequest, rule: SizeRule) -> CheckerCtx {
        let mut cfg = RepoConfig::default();
        cfg.hygiene.size = rule;
        CheckerCtx {
            gh: Arc::new(GitHub::new(reqwest::Client::new(), "t".into())),
            repo_cfg: Arc::new(cfg),
            owner: "o".into(),
            repo: "r".into(),
            pr: Arc::new(pr),
            files: vec![],
            prior_bot_reviews: vec![],
            prior_bot_comments: vec![],
            store: crate::storage::Store::in_memory().await.unwrap(),
            installation_id: None,
            job_id: 0,
        }
    }

    #[tokio::test]
    async fn small_pr_success() {
        let out = SizeChecker
            .run(&ctx_for(pr(10, 5, 2), SizeRule::default()).await)
            .await
            .unwrap();
        assert!(matches!(out.status, OutcomeStatus::Success));
    }

    #[tokio::test]
    async fn large_pr_warns_neutral() {
        let out = SizeChecker
            .run(&ctx_for(pr(600, 0, 30), SizeRule::default()).await)
            .await
            .unwrap();
        assert!(matches!(out.status, OutcomeStatus::Neutral));
    }
}
