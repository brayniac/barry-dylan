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
pub struct GitRef {
    pub sha: String,
    #[serde(rename = "ref")]
    pub r#ref: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct User {
    pub login: String,
}

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
    pub async fn get_pr(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> Result<PullRequest, GhError> {
        self.get_json(&format!("/repos/{owner}/{repo}/pulls/{number}"))
            .await
    }

    pub async fn list_pr_files(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> Result<Vec<ChangedFile>, GhError> {
        let mut out = Vec::new();
        let mut page = 1u32;
        loop {
            let page_files: Vec<ChangedFile> = self
                .get_json(&format!(
                    "/repos/{owner}/{repo}/pulls/{number}/files?per_page=100&page={page}"
                ))
                .await?;
            if page_files.is_empty() {
                break;
            }
            let last_full = page_files.len() == 100;
            out.extend(page_files);
            if !last_full {
                break;
            }
            page += 1;
            if page > 30 {
                break;
            } // safety cap (3000 files)
        }
        Ok(out)
    }

    /// Cached variant of `author_permission_raw`. Checks an in-memory TTL cache
    /// (5 min) on `(owner, user)` before issuing the REST call.
    pub async fn author_permission(
        &self,
        owner: &str,
        repo: &str,
        login: &str,
    ) -> Result<String, GhError> {
        // Fast path: cache hit.
        if let Some(perm) = self.perm_cache().get(owner, login) {
            return Ok(perm);
        }

        // Cache miss: fetch from GitHub, then store.
        let r: PermissionResp = self
            .get_json(&format!(
                "/repos/{owner}/{repo}/collaborators/{login}/permission"
            ))
            .await?;
        let perm = r.permission.clone();
        self.perm_cache().put(owner, login, perm);
        Ok(r.permission)
    }

    /// Raw (uncached) lookup of a user's permission on a repository.
    pub async fn author_permission_raw(
        &self,
        owner: &str,
        repo: &str,
        login: &str,
    ) -> Result<String, GhError> {
        let r: PermissionResp = self
            .get_json(&format!(
                "/repos/{owner}/{repo}/collaborators/{login}/permission"
            ))
            .await?;
        Ok(r.permission)
    }

    /// Fetch the .barry.toml at the repo's default branch; returns None if 404.
    pub async fn get_repo_config_text(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<Option<String>, GhError> {
        let path = format!("/repos/{owner}/{repo}/contents/.barry.toml?ref={branch}");
        match self.get_json::<ContentResp>(&path).await {
            Ok(c) => {
                use base64::Engine;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(c.content.replace('\n', ""))
                    .map_err(|e| GhError::Api {
                        status: 500,
                        body: e.to_string(),
                    })?;
                Ok(Some(String::from_utf8(bytes).map_err(|e| {
                    GhError::Api {
                        status: 500,
                        body: e.to_string(),
                    }
                })?))
            }
            Err(GhError::Api { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn list_pr_comments(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> Result<Vec<BotComment>, GhError> {
        let v: Vec<ListedComment> = self
            .get_json(&format!(
                "/repos/{owner}/{repo}/issues/{number}/comments?per_page=100"
            ))
            .await?;
        Ok(v.into_iter()
            .map(|c| BotComment {
                id: c.id,
                node_id: c.node_id,
                body: c.body,
                author: c.user.login,
            })
            .collect())
    }

    pub async fn list_pr_reviews(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> Result<Vec<BotComment>, GhError> {
        let v: Vec<ListedReview> = self
            .get_json(&format!(
                "/repos/{owner}/{repo}/pulls/{number}/reviews?per_page=100"
            ))
            .await?;
        Ok(v.into_iter()
            .map(|r| BotComment {
                id: r.id,
                node_id: r.node_id,
                body: r.body.unwrap_or_default(),
                author: r.user.login,
            })
            .collect())
    }

    /// One GraphQL round trip for PR metadata, the last 100 comments and reviews, and
    /// the `.barry.toml` blob at HEAD. Replaces four separate REST calls in setup.
    pub async fn fetch_pr_context(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> Result<PrContext, GhError> {
        let q = r#"
            query($owner: String!, $name: String!, $number: Int!) {
              repository(owner: $owner, name: $name) {
                pullRequest(number: $number) {
                  number title body state isDraft
                  additions deletions changedFiles
                  author { login }
                  headRefOid headRefName baseRefOid baseRefName
                  comments(last: 100) {
                    nodes { databaseId id author { login } body }
                  }
                  reviews(last: 100) {
                    nodes { databaseId id author { login } body }
                  }
                }
                config: object(expression: "HEAD:.barry.toml") {
                  ... on Blob { text }
                }
              }
            }
        "#;
        let body: PrCtxResponse = self
            .graphql(
                q,
                serde_json::json!({
                    "owner": owner, "name": repo, "number": number,
                }),
            )
            .await?;
        Ok(body.into_context())
    }
}

// --- GraphQL fetch_pr_context types ---

#[derive(Debug, Clone)]
pub struct PrContext {
    pub pr: PullRequest,
    pub comments: Vec<BotComment>,
    pub reviews: Vec<BotComment>,
    pub config_text: Option<String>,
}

#[derive(Deserialize)]
struct PrCtxResponse {
    data: PrCtxData,
}

#[derive(Deserialize)]
struct PrCtxData {
    repository: PrCtxRepository,
}

#[derive(Deserialize)]
struct PrCtxRepository {
    #[serde(rename = "pullRequest")]
    pull_request: PrCtxPullRequest,
    config: Option<PrCtxConfig>,
}

#[derive(Deserialize)]
struct PrCtxConfig {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct PrCtxPullRequest {
    number: i64,
    title: String,
    body: Option<String>,
    state: String,
    #[serde(rename = "isDraft")]
    is_draft: bool,
    additions: i64,
    deletions: i64,
    #[serde(rename = "changedFiles")]
    changed_files: i64,
    author: Option<PrCtxActor>,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    #[serde(rename = "baseRefOid")]
    base_ref_oid: String,
    #[serde(rename = "baseRefName")]
    base_ref_name: String,
    comments: PrCtxConnection<PrCtxComment>,
    reviews: PrCtxConnection<PrCtxReview>,
}

#[derive(Deserialize)]
struct PrCtxConnection<T> {
    nodes: Vec<T>,
}

#[derive(Deserialize)]
struct PrCtxActor {
    login: String,
}

#[derive(Deserialize)]
struct PrCtxComment {
    #[serde(rename = "databaseId")]
    database_id: i64,
    id: String,
    author: Option<PrCtxActor>,
    // GitHub's schema declares this `String!`, but we accept null defensively:
    // a single bad payload shouldn't put the worker in a retry loop for the PR.
    body: Option<String>,
}

#[derive(Deserialize)]
struct PrCtxReview {
    #[serde(rename = "databaseId")]
    database_id: i64,
    id: String,
    author: Option<PrCtxActor>,
    body: Option<String>,
}

impl PrCtxResponse {
    fn into_context(self) -> PrContext {
        let p = self.data.repository.pull_request;
        let author_login = p.author.map(|a| a.login).unwrap_or_else(|| "ghost".into());
        let pr = PullRequest {
            number: p.number,
            title: p.title,
            body: p.body,
            user: User {
                login: author_login,
            },
            draft: p.is_draft,
            state: p.state.to_lowercase(),
            head: GitRef {
                sha: p.head_ref_oid,
                r#ref: p.head_ref_name,
            },
            base: GitRef {
                sha: p.base_ref_oid,
                r#ref: p.base_ref_name,
            },
            additions: p.additions,
            deletions: p.deletions,
            changed_files: p.changed_files,
        };
        let comments = p
            .comments
            .nodes
            .into_iter()
            .map(|c| BotComment {
                id: c.database_id,
                node_id: c.id,
                body: c.body.unwrap_or_default(),
                author: c.author.map(|a| a.login).unwrap_or_default(),
            })
            .collect();
        let reviews = p
            .reviews
            .nodes
            .into_iter()
            .map(|r| BotComment {
                id: r.database_id,
                node_id: r.id,
                body: r.body.unwrap_or_default(),
                author: r.author.map(|a| a.login).unwrap_or_default(),
            })
            .collect();
        let config_text = self.data.repository.config.and_then(|c| c.text);
        PrContext {
            pr,
            comments,
            reviews,
            config_text,
        }
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
        &self,
        owner: &str,
        repo: &str,
        number: i64,
        input: &ReviewInput<'_>,
    ) -> Result<i64, GhError> {
        #[derive(Deserialize)]
        struct R {
            id: i64,
        }
        let path = format!("/repos/{owner}/{repo}/pulls/{number}/reviews");
        let r: R = self
            .post_json(&path, serde_json::to_value(input).unwrap())
            .await?;
        Ok(r.id)
    }

    pub async fn create_issue_comment(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
        body: &str,
    ) -> Result<i64, GhError> {
        #[derive(Deserialize)]
        struct R {
            id: i64,
        }
        let path = format!("/repos/{owner}/{repo}/issues/{number}/comments");
        let r: R = self
            .post_json(&path, serde_json::json!({ "body": body }))
            .await?;
        Ok(r.id)
    }

    pub async fn react(
        &self,
        owner: &str,
        repo: &str,
        comment_id: i64,
        content: &str,
    ) -> Result<(), GhError> {
        let path = format!("/repos/{owner}/{repo}/issues/comments/{comment_id}/reactions");
        let _: serde_json::Value = self
            .post_json(&path, serde_json::json!({ "content": content }))
            .await?;
        Ok(())
    }

    pub async fn add_labels(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
        labels: &[String],
    ) -> Result<(), GhError> {
        let path = format!("/repos/{owner}/{repo}/issues/{number}/labels");
        let _: serde_json::Value = self
            .post_json(&path, serde_json::json!({ "labels": labels }))
            .await?;
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
        let _: serde_json::Value = self
            .graphql(q, serde_json::json!({ "id": node_id }))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_context_parses_graphql_response() {
        let raw = serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "number": 42, "title": "feat: x", "body": "body",
                        "state": "OPEN", "isDraft": false,
                        "additions": 10, "deletions": 2, "changedFiles": 3,
                        "author": { "login": "alice" },
                        "headRefOid": "h1", "headRefName": "feat-x",
                        "baseRefOid": "b1", "baseRefName": "main",
                        "comments": { "nodes": [
                            {"databaseId": 1, "id": "IC_1", "author": {"login": "bob"}, "body": "hi"},
                            {"databaseId": 2, "id": "IC_2", "author": null, "body": "ghost"}
                        ]},
                        "reviews": { "nodes": [
                            {"databaseId": 3, "id": "PR_1", "author": {"login": "barry-dylan"}, "body": "ok"}
                        ]}
                    },
                    "config": { "text": "[hygiene]\nenabled = true\n" }
                }
            }
        });
        let parsed: PrCtxResponse = serde_json::from_value(raw).unwrap();
        let ctx = parsed.into_context();
        assert_eq!(ctx.pr.number, 42);
        assert_eq!(ctx.pr.state, "open"); // lowercased
        assert_eq!(ctx.pr.user.login, "alice");
        assert_eq!(ctx.pr.head.sha, "h1");
        assert_eq!(ctx.pr.base.r#ref, "main");
        assert_eq!(ctx.comments.len(), 2);
        assert_eq!(ctx.comments[1].author, ""); // null author
        assert_eq!(ctx.reviews.len(), 1);
        assert_eq!(ctx.reviews[0].author, "barry-dylan");
        assert_eq!(
            ctx.config_text.as_deref(),
            Some("[hygiene]\nenabled = true\n")
        );
    }

    #[test]
    fn pr_context_handles_null_comment_body() {
        // GitHub's schema declares IssueComment.body non-null, but we accept
        // null defensively so a bad payload doesn't kill the job for that PR.
        let raw = serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "number": 1, "title": "t", "body": "b",
                        "state": "OPEN", "isDraft": false,
                        "additions": 0, "deletions": 0, "changedFiles": 0,
                        "author": { "login": "a" },
                        "headRefOid": "h", "headRefName": "x",
                        "baseRefOid": "b", "baseRefName": "main",
                        "comments": { "nodes": [
                            {"databaseId": 9, "id": "IC_9", "author": {"login": "x"}, "body": null}
                        ]},
                        "reviews": { "nodes": [] }
                    },
                    "config": null
                }
            }
        });
        let ctx = serde_json::from_value::<PrCtxResponse>(raw)
            .unwrap()
            .into_context();
        assert_eq!(ctx.comments.len(), 1);
        assert_eq!(ctx.comments[0].body, "");
    }

    #[test]
    fn pr_context_handles_null_config_and_null_author() {
        let raw = serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "number": 1, "title": "t", "body": null,
                        "state": "CLOSED", "isDraft": true,
                        "additions": 0, "deletions": 0, "changedFiles": 0,
                        "author": null,
                        "headRefOid": "h", "headRefName": "x",
                        "baseRefOid": "b", "baseRefName": "main",
                        "comments": { "nodes": [] },
                        "reviews": { "nodes": [] }
                    },
                    "config": null
                }
            }
        });
        let parsed: PrCtxResponse = serde_json::from_value(raw).unwrap();
        let ctx = parsed.into_context();
        assert_eq!(ctx.pr.user.login, "ghost");
        assert!(ctx.config_text.is_none());
    }
}
