use crate::github::client::{GhError, GitHub};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Clone)]
pub struct PullRequest {
    pub number: i64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub user: User,
    pub draft: bool,
    pub state: String,
    pub head: GitRef,
    pub base: GitRef,
    pub additions: i64,
    pub deletions: i64,
    pub changed_files: i64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GitRef { pub sha: String, #[serde(rename = "ref")] pub r#ref: String }

#[derive(Debug, Deserialize, Clone)]
pub struct User { pub login: String }

#[derive(Debug, Deserialize, Clone)]
pub struct ChangedFile {
    pub filename: String,
    pub status: String,
    pub additions: i64,
    pub deletions: i64,
    pub changes: i64,
    #[serde(default)]
    pub patch: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PermissionResp {
    pub permission: String,
}

#[derive(Debug, Deserialize)]
pub struct ContentResp {
    pub content: String, // base64
    pub encoding: String,
}

#[derive(Debug, Deserialize)]
struct ListedComment {
    pub id: i64,
    pub body: String,
    pub user: User,
    pub node_id: String,
}

#[derive(Debug, Deserialize)]
struct ListedReview {
    pub id: i64,
    pub body: Option<String>,
    pub user: User,
    pub node_id: String,
}

impl GitHub {
    pub async fn get_pr(&self, owner: &str, repo: &str, number: i64) -> Result<PullRequest, GhError> {
        self.get_json(&format!("/repos/{owner}/{repo}/pulls/{number}")).await
    }

    pub async fn list_pr_files(&self, owner: &str, repo: &str, number: i64) -> Result<Vec<ChangedFile>, GhError> {
        let mut out = Vec::new();
        let mut page = 1u32;
        loop {
            let page_files: Vec<ChangedFile> =
                self.get_json(&format!("/repos/{owner}/{repo}/pulls/{number}/files?per_page=100&page={page}")).await?;
            if page_files.is_empty() { break; }
            let last_full = page_files.len() == 100;
            out.extend(page_files);
            if !last_full { break; }
            page += 1;
            if page > 30 { break; } // safety cap (3000 files)
        }
        Ok(out)
    }

    pub async fn author_permission(&self, owner: &str, repo: &str, login: &str)
        -> Result<String, GhError>
    {
        let r: PermissionResp = self
            .get_json(&format!("/repos/{owner}/{repo}/collaborators/{login}/permission")).await?;
        Ok(r.permission)
    }

    /// Fetch the .barry.toml at the repo's default branch; returns None if 404.
    pub async fn get_repo_config_text(&self, owner: &str, repo: &str, branch: &str)
        -> Result<Option<String>, GhError>
    {
        let path = format!("/repos/{owner}/{repo}/contents/.barry.toml?ref={branch}");
        match self.get_json::<ContentResp>(&path).await {
            Ok(c) => {
                use base64::Engine;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(c.content.replace('\n', ""))
                    .map_err(|e| GhError::Api { status: 500, body: e.to_string() })?;
                Ok(Some(String::from_utf8(bytes)
                    .map_err(|e| GhError::Api { status: 500, body: e.to_string() })?))
            }
            Err(GhError::Api { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn list_pr_comments(&self, owner: &str, repo: &str, number: i64)
        -> Result<Vec<BotComment>, GhError>
    {
        let v: Vec<ListedComment> = self
            .get_json(&format!("/repos/{owner}/{repo}/issues/{number}/comments?per_page=100")).await?;
        Ok(v.into_iter().map(|c| BotComment {
            id: c.id, node_id: c.node_id, body: c.body, author: c.user.login,
        }).collect())
    }

    pub async fn list_pr_reviews(&self, owner: &str, repo: &str, number: i64)
        -> Result<Vec<BotComment>, GhError>
    {
        let v: Vec<ListedReview> = self
            .get_json(&format!("/repos/{owner}/{repo}/pulls/{number}/reviews?per_page=100")).await?;
        Ok(v.into_iter().map(|r| BotComment {
            id: r.id, node_id: r.node_id, body: r.body.unwrap_or_default(), author: r.user.login,
        }).collect())
    }
}

#[derive(Debug, Clone)]
pub struct BotComment {
    pub id: i64,
    pub node_id: String,
    pub body: String,
    pub author: String,
}

// --- Task 15: write endpoints ---

#[derive(Debug, Clone, Serialize)]
pub struct ReviewCommentInput {
    pub path: String,
    pub position: i64,
    pub body: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewInput<'a> {
    pub body: &'a str,
    pub event: &'static str,
    pub comments: &'a [ReviewCommentInput],
    pub commit_id: &'a str,
}

impl GitHub {
    pub async fn create_review(
        &self, owner: &str, repo: &str, number: i64, input: &ReviewInput<'_>,
    ) -> Result<i64, GhError> {
        #[derive(Deserialize)] struct R { id: i64 }
        let path = format!("/repos/{owner}/{repo}/pulls/{number}/reviews");
        let r: R = self.post_json(&path, serde_json::to_value(input).unwrap()).await?;
        Ok(r.id)
    }

    pub async fn create_issue_comment(
        &self, owner: &str, repo: &str, number: i64, body: &str,
    ) -> Result<i64, GhError> {
        #[derive(Deserialize)] struct R { id: i64 }
        let path = format!("/repos/{owner}/{repo}/issues/{number}/comments");
        let r: R = self.post_json(&path, serde_json::json!({ "body": body })).await?;
        Ok(r.id)
    }

    pub async fn react(
        &self, owner: &str, repo: &str, comment_id: i64, content: &str,
    ) -> Result<(), GhError> {
        let path = format!("/repos/{owner}/{repo}/issues/comments/{comment_id}/reactions");
        let _: serde_json::Value = self.post_json(&path, serde_json::json!({ "content": content })).await?;
        Ok(())
    }

    pub async fn add_labels(
        &self, owner: &str, repo: &str, number: i64, labels: &[String],
    ) -> Result<(), GhError> {
        let path = format!("/repos/{owner}/{repo}/issues/{number}/labels");
        let _: serde_json::Value = self.post_json(&path, serde_json::json!({ "labels": labels })).await?;
        Ok(())
    }

    /// Minimize a comment/review via GraphQL `minimizeComment` mutation.
    pub async fn minimize_comment(&self, node_id: &str) -> Result<(), GhError> {
        let q = r#"
            mutation($id: ID!) {
              minimizeComment(input: { subjectId: $id, classifier: OUTDATED }) {
                minimizedComment { isMinimized }
              }
            }
        "#;
        let _: serde_json::Value =
            self.graphql(q, serde_json::json!({ "id": node_id })).await?;
        Ok(())
    }
}
