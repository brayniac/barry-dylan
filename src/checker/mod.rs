//! PR checkers for hygiene and multi-review LLM.
//!
//! # Overview
//!
//! Checkers are the core of Barry Dylan's functionality. Each checker examines
//! a pull request and produces an outcome:
//!
//! - **Hygiene checkers**: PR metadata validation (title format, description,
//!   size warnings, autolabels)
//! - **Multi-review checker**: Parallel LLM reviews from multiple providers
//!   with a hidden judge to decide agreement
//!
//! # Architecture
//!
//! ## The `Checker` Trait
//!
//! ```ignore
//! pub trait Checker: Send + Sync {
//!     fn name(&self) -> &'static str;              // Checker name for logging/metrics
//!     fn enabled(&self, cfg: &RepoConfig) -> bool; // Per-repo enablement
//!     async fn run(&self, ctx: &CheckerCtx) -> Result<CheckerOutcome>;
//! }
//! ```
//!
//! ## The `CheckerCtx`
//!
//! Context passed to each checker contains:
//! - GitHub client for API access
//! - Repo configuration (from `.barry.toml`)
//! - PR metadata and files
//! - Prior bot comments/reviews (for deduplication)
//! - Database store
//!
//! ## The `CheckerOutcome`
//!
//! Outcomes include:
//! - `status`: Success, Neutral, or Failure
//! - `summary`: Brief summary for the check run
//! - `text`: Detailed markdown text (for collapsed sections)
//! - `inline_comments`: PR review comments to post
//! - `issue_comment`: Additional issue comment (one-shot notices)
//! - `add_labels`: Labels to add to the PR
//!
//! # Implementation Patterns
//!
//! ## Hygiene Checkers
//!
//! Hygiene checkers are simple, fast validations:
//! - Title matches regex pattern
//! - Description has minimum length
//! - PR size is reasonable
//! - Auto-label based on file patterns
//!
//! ## Multi-Review Checker
//!
//! The `MultiReviewChecker` runs parallel LLM reviews:
//! 1. Fetch PR context (files, comments, reviews)
//! 2. Run each LLM provider's review in parallel
//! 3. Send diffs to judge LLM
//! 4. If judge says "agree" → Barry posts unified review
//! 5. If judge says "disagree" → Both providers post
//!
//! ## LLM Client Abstraction
//!
//! LLM clients implement the `LlmClient` trait:
//! ```ignore
//! pub trait LlmClient: Send + Sync {
//!     async fn complete(&self, req: &LlmRequest) -> Result<LlmResponse, LlmError>;
//! }
//! ```
//!
//! The `retry_transient` helper handles connection errors and 5xx status codes
//! with exponential backoff.

pub mod hygiene;
pub mod multi_review;

use crate::config::repo::RepoConfig;
use crate::github::client::GitHub;
use crate::github::pr::{BotComment, ChangedFile, PullRequest};
use async_trait::async_trait;
use std::sync::Arc;

/// A code finding from a checker.
#[derive(Debug, Clone)]
pub struct Finding {
    pub file: String,
    pub line: Option<u32>,
    pub message: String,
}

/// Status of a checker's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeStatus {
    /// Checker passed successfully.
    Success,
    /// Checker ran but found no issues.
    Neutral,
    /// Checker found issues that should block the PR.
    Failure,
}

/// Result of running a checker on a PR.
#[derive(Debug, Clone)]
pub struct CheckerOutcome {
    pub checker_name: &'static str,
    pub status: OutcomeStatus,
    pub summary: String,
    pub text: Option<String>,
    /// PR-Review inline comments.
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

/// Context passed to checkers for running on a PR.
pub struct CheckerCtx {
    pub gh: Arc<GitHub>,
    pub repo_cfg: Arc<RepoConfig>,
    pub owner: String,
    pub repo: String,
    pub pr: Arc<PullRequest>,
    pub files: Vec<ChangedFile>,
    pub prior_bot_reviews: Vec<BotComment>,
    pub prior_bot_comments: Vec<BotComment>,
    pub store: crate::storage::Store,
    pub installation_id: Option<i64>,
    pub job_id: i64,
}

/// Trait implemented by all checkers.
#[async_trait]
pub trait Checker: Send + Sync {
    /// Name of the checker (used for logging and metrics).
    fn name(&self) -> &'static str;
    /// Whether this checker is enabled for the given repo config.
    fn enabled(&self, cfg: &RepoConfig) -> bool;
    /// Run the checker on the PR and return an outcome.
    async fn run(&self, ctx: &CheckerCtx) -> anyhow::Result<CheckerOutcome>;
}
