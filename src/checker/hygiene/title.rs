use crate::checker::{Checker, CheckerCtx, CheckerOutcome};
use crate::config::repo::RepoConfig;
use async_trait::async_trait;
use regex::Regex;

pub struct TitleChecker;

#[async_trait]
impl Checker for TitleChecker {
    fn name(&self) -> &'static str {
        "barry/hygiene.title"
    }
    fn enabled(&self, cfg: &RepoConfig) -> bool {
        cfg.hygiene.title.enabled
    }
    async fn run(&self, ctx: &CheckerCtx) -> anyhow::Result<CheckerOutcome> {
        let rule = &ctx.repo_cfg.hygiene.title;
        let re =
            Regex::new(&rule.pattern).map_err(|e| anyhow::anyhow!("invalid title pattern: {e}"))?;
        let ok = re.is_match(&ctx.pr.title);
        Ok(if ok {
            CheckerOutcome::success(self.name(), "title matches required format")
        } else {
            CheckerOutcome::failure(
                self.name(),
                format!("title does not match required pattern `{}`", rule.pattern),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::repo::TitleRule;
    use crate::github::client::GitHub;
    use crate::github::pr::{GitRef, PullRequest, User};
    use std::sync::Arc;

    fn ctx_with(title: &str, rule: TitleRule) -> CheckerCtx {
        let mut cfg = RepoConfig::default();
        cfg.hygiene.title = rule;
        CheckerCtx {
            gh: Arc::new(GitHub::new(reqwest::Client::new(), "t".into())),
            repo_cfg: Arc::new(cfg),
            owner: "o".into(),
            repo: "r".into(),
            pr: PullRequest {
                number: 1,
                title: title.into(),
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
                additions: 0,
                deletions: 0,
                changed_files: 0,
            },
            files: vec![],
            prior_bot_reviews: vec![],
            prior_bot_comments: vec![],
        }
    }

    #[tokio::test]
    async fn good_title_passes() {
        let ctx = ctx_with("feat: add foo", TitleRule::default());
        let out = TitleChecker.run(&ctx).await.unwrap();
        assert!(matches!(out.status, crate::checker::OutcomeStatus::Success));
    }

    #[tokio::test]
    async fn bad_title_fails() {
        let ctx = ctx_with("untyped change", TitleRule::default());
        let out = TitleChecker.run(&ctx).await.unwrap();
        assert!(matches!(out.status, crate::checker::OutcomeStatus::Failure));
    }

    #[tokio::test]
    async fn ci_build_style_revert_pass() {
        for title in [
            "ci: add github actions workflow",
            "build: bump rust edition",
            "style: cargo fmt across codebase",
            "revert: undo experimental change",
        ] {
            let ctx = ctx_with(title, TitleRule::default());
            let out = TitleChecker.run(&ctx).await.unwrap();
            assert!(
                matches!(out.status, crate::checker::OutcomeStatus::Success),
                "expected `{title}` to pass",
            );
        }
    }

    #[tokio::test]
    async fn breaking_change_marker_passes() {
        for title in ["feat!: drop legacy api", "refactor(api)!: rename endpoints"] {
            let ctx = ctx_with(title, TitleRule::default());
            let out = TitleChecker.run(&ctx).await.unwrap();
            assert!(
                matches!(out.status, crate::checker::OutcomeStatus::Success),
                "expected `{title}` to pass",
            );
        }
    }
}
