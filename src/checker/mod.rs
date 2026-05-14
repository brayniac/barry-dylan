pub mod hygiene;
pub mod llm_review;
pub mod multi_review;

use crate::config::repo::RepoConfig;
use crate::github::client::GitHub;
use crate::github::pr::{BotComment, ChangedFile, PullRequest};
use async_trait::async_trait;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Finding {
    pub file: String,
    pub line: Option<u32>,
    pub message: String,
}

#[derive(Debug, Clone, Copy)]
pub enum OutcomeStatus {
    Success,
    Neutral,
    Failure,
}

#[derive(Debug, Clone)]
pub struct CheckerOutcome {
    pub checker_name: &'static str,
    pub status: OutcomeStatus,
    pub summary: String,
    pub text: Option<String>,
    /// PR-Review inline comments (only LlmReview uses these).
    pub inline_comments: Vec<crate::github::pr::ReviewCommentInput>,
    /// Issue-comment to post in addition (used only for one-shot notices).
    pub issue_comment: Option<String>,
    /// Labels to add.
    pub add_labels: Vec<String>,
}

impl CheckerOutcome {
    pub fn neutral(name: &'static str, summary: impl Into<String>) -> Self {
        Self {
            checker_name: name,
            status: OutcomeStatus::Neutral,
            summary: summary.into(),
            text: None,
            inline_comments: vec![],
            issue_comment: None,
            add_labels: vec![],
        }
    }
    pub fn success(name: &'static str, summary: impl Into<String>) -> Self {
        Self {
            status: OutcomeStatus::Success,
            ..Self::neutral(name, summary)
        }
    }
    pub fn failure(name: &'static str, summary: impl Into<String>) -> Self {
        Self {
            status: OutcomeStatus::Failure,
            ..Self::neutral(name, summary)
        }
    }
}

pub struct CheckerCtx {
    pub gh: Arc<GitHub>,
    pub repo_cfg: Arc<RepoConfig>,
    pub owner: String,
    pub repo: String,
    pub pr: PullRequest,
    pub files: Vec<ChangedFile>,
    pub prior_bot_reviews: Vec<BotComment>,
    pub prior_bot_comments: Vec<BotComment>,
}

#[async_trait]
pub trait Checker: Send + Sync {
    fn name(&self) -> &'static str;
    fn enabled(&self, cfg: &RepoConfig) -> bool;
    async fn run(&self, ctx: &CheckerCtx) -> anyhow::Result<CheckerOutcome>;
}
