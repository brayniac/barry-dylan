use crate::github::client::{GhError, GitHub};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct CheckRunInput {
    pub name: String,
    pub head_sha: String,
    pub status: CheckStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<CheckConclusion>,
    pub output: CheckOutput,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Queued,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckConclusion {
    Success,
    Failure,
    Neutral,
    Cancelled,
    TimedOut,
    ActionRequired,
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckOutput {
    pub title: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CheckRunResp {
    pub id: i64,
}

impl GitHub {
    /// Create a Check Run. GitHub shows only the latest check run of each
    /// name on a head, so re-using `name` for the same head_sha supersedes
    /// the previous one in the PR's checks.
    ///
    /// Retried on transient failures without checking whether an earlier
    /// attempt landed. A retry of an attempt GitHub had applied creates a
    /// second run of the same name with the same result, and only the latest
    /// is shown. Returns `None` when GitHub accepted the run but its response
    /// could not be read.
    pub async fn create_check_run(
        &self,
        owner: &str,
        repo: &str,
        input: &CheckRunInput,
    ) -> Result<Option<CheckRunResp>, GhError> {
        let path = format!("/repos/{owner}/{repo}/check-runs");
        self.write_json(
            "check_run",
            reqwest::Method::POST,
            &path,
            serde_json::to_value(input).unwrap(),
        )
        .await
    }
}
