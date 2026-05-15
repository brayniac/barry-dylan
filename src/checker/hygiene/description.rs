use crate::checker::{Checker, CheckerCtx, CheckerOutcome};
use crate::config::repo::RepoConfig;
use async_trait::async_trait;

pub struct DescriptionChecker;

#[async_trait]
impl Checker for DescriptionChecker {
    fn name(&self) -> &'static str {
        "barry/hygiene.description"
    }
    fn enabled(&self, cfg: &RepoConfig) -> bool {
        cfg.hygiene.description.enabled
    }
    async fn run(&self, ctx: &CheckerCtx) -> anyhow::Result<CheckerOutcome> {
        let rule = &ctx.repo_cfg.hygiene.description;
        let body = ctx.pr.body.as_deref().unwrap_or("");
        let trimmed = body.trim();
        if trimmed.len() < rule.min_length {
            return Ok(CheckerOutcome::failure(
                self.name(),
                format!(
                    "description too short ({}/{} chars)",
                    trimmed.len(),
                    rule.min_length
                ),
            ));
        }
        let missing: Vec<_> = rule
            .require_template_sections
            .iter()
            .filter(|s| !body.contains(s.as_str()))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Ok(CheckerOutcome::failure(
                self.name(),
                format!("missing required sections: {}", missing.join(", ")),
            ));
        }
        Ok(CheckerOutcome::success(self.name(), "description ok"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::repo::DescriptionRule;
    use crate::github::client::GitHub;
    use crate::github::pr::{GitRef, PullRequest, User};
    use std::sync::Arc;

    async fn ctx(body: Option<&str>, rule: DescriptionRule) -> CheckerCtx {
        let mut cfg = RepoConfig::default();
        cfg.hygiene.description = rule;
        CheckerCtx {
            gh: Arc::new(GitHub::new(reqwest::Client::new(), "t".into())),
            repo_cfg: Arc::new(cfg),
            owner: "o".into(),
            repo: "r".into(),
            pr: PullRequest {
                number: 1,
                title: "t".into(),
                body: body.map(str::to_string),
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
            store: crate::storage::Store::in_memory().await.unwrap(),
            installation_id: None,
        }
    }

    #[tokio::test]
    async fn short_body_fails() {
        let out = DescriptionChecker
            .run(&ctx(Some("hi"), DescriptionRule::default()).await)
            .await
            .unwrap();
        assert!(matches!(out.status, crate::checker::OutcomeStatus::Failure));
    }

    #[tokio::test]
    async fn long_body_passes() {
        let out = DescriptionChecker
            .run(
                &ctx(
                    Some("This is plenty long for the default rule."),
                    DescriptionRule::default(),
                )
                .await,
            )
            .await
            .unwrap();
        assert!(matches!(out.status, crate::checker::OutcomeStatus::Success));
    }

    #[tokio::test]
    async fn missing_section_fails() {
        let r = DescriptionRule {
            min_length: 0,
            require_template_sections: vec!["## Test plan".into()],
            ..DescriptionRule::default()
        };
        let out = DescriptionChecker
            .run(&ctx(Some("body without section"), r).await)
            .await
            .unwrap();
        assert!(matches!(out.status, crate::checker::OutcomeStatus::Failure));
    }
}
