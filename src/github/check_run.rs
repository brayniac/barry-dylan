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
pub enum CheckStatus { Queued, InProgress, Completed }

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckConclusion { Success, Failure, Neutral, Cancelled, TimedOut, ActionRequired, Skipped }

#[derive(Debug, Clone, Serialize)]
pub struct CheckOutput {
    pub title: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CheckRunResp { pub id: i64 }

impl GitHub {
    /// Create a Check Run. Re-using the same `name` for the same head_sha replaces the previous one.
    pub async fn create_check_run(
        &self, owner: &str, repo: &str, input: &CheckRunInput,
    ) -> Result<CheckRunResp, GhError> {
        let path = format!("/repos/{owner}/{repo}/check-runs");
        self.post_json(&path, serde_json::to_value(input).unwrap()).await
    }
}
