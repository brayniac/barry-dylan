use crate::checker::{Checker, CheckerCtx, CheckerOutcome, OutcomeStatus};
use crate::config::repo::RepoConfig;
use async_trait::async_trait;
use globset::{Glob, GlobSetBuilder};
use std::collections::BTreeSet;

pub struct AutolabelChecker;

#[async_trait]
impl Checker for AutolabelChecker {
    fn name(&self) -> String {
        "barry/hygiene.autolabel".to_string()
    }
    fn enabled(&self, cfg: &RepoConfig) -> bool {
        cfg.hygiene.autolabel.enabled
    }
    async fn run(&self, ctx: &CheckerCtx) -> anyhow::Result<CheckerOutcome> {
        let mut to_add: BTreeSet<String> = BTreeSet::new();
        for rule in &ctx.repo_cfg.hygiene.autolabel.rules {
            let mut b = GlobSetBuilder::new();
            for p in &rule.paths {
                b.add(Glob::new(p)?);
            }
            let set = b.build()?;
            let any = ctx.files.iter().any(|f| set.is_match(&f.filename));
            if any {
                for l in &rule.labels {
                    to_add.insert(l.clone());
                }
            }
        }
        let labels: Vec<String> = to_add.into_iter().collect();
        let summary = if labels.is_empty() {
            "no labels to add".into()
        } else {
            format!("apply labels: {}", labels.join(", "))
        };
        Ok(CheckerOutcome {
            checker_name: self.name(),
            status: OutcomeStatus::Success,
            summary,
            text: None,
            inline_comments: vec![],
            issue_comment: None,
            add_labels: labels,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::repo::{Autolabel, AutolabelRule};
    use crate::github::client::GitHub;
    use crate::github::pr::{ChangedFile, GitRef, PullRequest, User};
    use std::sync::Arc;

    async fn ctx(files: Vec<&str>, rules: Vec<AutolabelRule>) -> CheckerCtx {
        let mut cfg = RepoConfig::default();
        cfg.hygiene.autolabel = Autolabel {
            enabled: true,
            rules,
        };
        let files = files
            .into_iter()
            .map(|f| ChangedFile {
                filename: f.into(),
                status: "modified".into(),
                additions: 1,
                deletions: 0,
                changes: 1,
                patch: None,
            })
            .collect();
        CheckerCtx {
            gh: Arc::new(GitHub::new(reqwest::Client::new(), "t".into())),
            repo_cfg: Arc::new(cfg),
            owner: "o".into(),
            repo: "r".into(),
            pr: PullRequest {
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
                additions: 1,
                deletions: 0,
                changed_files: 1,
            },
            files,
            prior_bot_reviews: vec![],
            prior_bot_comments: vec![],
            store: crate::storage::Store::in_memory().await.unwrap(),
            installation_id: None,
        }
    }

    #[tokio::test]
    async fn matching_rule_adds_labels() {
        let c = ctx(
            vec!["src/x/foo.rs"],
            vec![AutolabelRule {
                paths: vec!["src/x/**".into()],
                labels: vec!["area/x".into()],
            }],
        )
        .await;
        let out = AutolabelChecker.run(&c).await.unwrap();
        assert_eq!(out.add_labels, vec!["area/x"]);
    }

    #[tokio::test]
    async fn non_matching_rule_adds_nothing() {
        let c = ctx(
            vec!["docs/readme.md"],
            vec![AutolabelRule {
                paths: vec!["src/**".into()],
                labels: vec!["area/src".into()],
            }],
        )
        .await;
        let out = AutolabelChecker.run(&c).await.unwrap();
        assert!(out.add_labels.is_empty());
    }
}
