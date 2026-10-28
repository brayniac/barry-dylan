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

#[derive(Debug, Deserialize, Clone, Serialize)]
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

    /// Cached lookup that checks an in-memory TTL cache (5 min) on
    /// `(owner, repo, user)` before issuing the REST call.
    pub async fn author_permission(
        &self,
        owner: &str,
        repo: &str,
        login: &str,
    ) -> Result<String, GhError> {
        // Fast path: cache hit.
        if let Some(perm) = self.perm_cache().get(owner, repo, login) {
            return Ok(perm);
        }

        // Cache miss: delegate to raw lookup and cache the result.
        let perm = self.author_permission_raw(owner, repo, login).await?;
        self.perm_cache().put(owner, repo, login, perm.clone());
        Ok(perm)
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
                    nodes { databaseId id author { __typename login } body }
                  }
                  reviews(last: 100) {
                    nodes { databaseId id author { __typename login } body }
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
    /// `Bot`, `User`, ...; requested on comment and review authors only.
    #[serde(rename = "__typename", default)]
    typename: Option<String>,
}

impl PrCtxActor {
    /// The login as REST spells it. GraphQL gives a bot's login without the
    /// `[bot]` suffix, so a user account and an App's bot can share a GraphQL
    /// login; the suffix, which a user login cannot contain, tells them apart.
    fn rest_login(self) -> String {
        match self.typename.as_deref() {
            Some("Bot") => format!("{}[bot]", self.login),
            _ => self.login,
        }
    }
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
                author: c.author.map(PrCtxActor::rest_login).unwrap_or_default(),
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
                author: r.author.map(PrCtxActor::rest_login).unwrap_or_default(),
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
    /// The author's login as REST spells it: an App's bot is `<slug>[bot]`.
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

/// How far before a write's first attempt a copy of it found by
/// [`GitHub::find_recent_review`] or [`GitHub::find_recent_comment`] may have
/// been created, to allow for this host's clock running ahead of GitHub's.
/// A copy older than that is an earlier post of the same text, not this one.
const LANDED_CLOCK_SLACK: time::Duration = time::Duration::minutes(2);

impl GitHub {
    /// Post a review.
    ///
    /// A review sent twice is posted twice, and GitHub has been seen to
    /// record a review and still answer 502. So after a 5xx or a transport
    /// error that may have reached GitHub, the review is resent only if
    /// [`GitHub::find_recent_review`] finds no review with the same body on
    /// the same commit created since this call began; one it finds is taken
    /// as this post. A refused connection or a rate limit is resent directly.
    pub async fn create_review(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
        input: &ReviewInput<'_>,
    ) -> Result<i64, GhError> {
        let path = format!("/repos/{owner}/{repo}/pulls/{number}/reviews");
        let since = time::OffsetDateTime::now_utc() - LANDED_CLOCK_SLACK;
        self.post_unique(
            "review",
            &path,
            serde_json::to_value(input).unwrap(),
            || self.find_recent_review(owner, repo, number, input.body, input.commit_id, since),
        )
        .await
    }

    /// Post an issue comment, guarded against a duplicate the way
    /// [`GitHub::create_review`] is, matching on body alone.
    pub async fn create_issue_comment(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
        body: &str,
    ) -> Result<i64, GhError> {
        let path = format!("/repos/{owner}/{repo}/issues/{number}/comments");
        let since = time::OffsetDateTime::now_utc() - LANDED_CLOCK_SLACK;
        self.post_unique(
            "issue_comment",
            &path,
            serde_json::json!({ "body": body }),
            || self.find_recent_comment(owner, repo, number, body, since),
        )
        .await
    }

    /// The id of a review among the PR's last 50 with this body, on this
    /// commit, created at or after `since`.
    pub async fn find_recent_review(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
        body: &str,
        commit: &str,
        since: time::OffsetDateTime,
    ) -> Result<Option<i64>, GhError> {
        let q = r#"
            query($owner: String!, $name: String!, $number: Int!) {
              repository(owner: $owner, name: $name) {
                pullRequest(number: $number) {
                  reviews(last: 50) {
                    nodes { databaseId body createdAt commit { oid } }
                  }
                }
              }
            }
        "#;
        let r: RecentResponse = self
            .graphql(
                q,
                serde_json::json!({ "owner": owner, "name": repo, "number": number }),
            )
            .await?;
        let nodes = r.data.repository.pull_request.reviews.unwrap_or_default();
        Ok(nodes
            .nodes
            .into_iter()
            .find(|n| n.matches(body, since) && n.commit.as_ref().is_some_and(|c| c.oid == commit))
            .map(|n| n.database_id))
    }

    /// The id of an issue comment among the PR's last 50 with this body,
    /// created at or after `since`.
    pub async fn find_recent_comment(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
        body: &str,
        since: time::OffsetDateTime,
    ) -> Result<Option<i64>, GhError> {
        let q = r#"
            query($owner: String!, $name: String!, $number: Int!) {
              repository(owner: $owner, name: $name) {
                pullRequest(number: $number) {
                  comments(last: 50) {
                    nodes { databaseId body createdAt }
                  }
                }
              }
            }
        "#;
        let r: RecentResponse = self
            .graphql(
                q,
                serde_json::json!({ "owner": owner, "name": repo, "number": number }),
            )
            .await?;
        let nodes = r.data.repository.pull_request.comments.unwrap_or_default();
        Ok(nodes
            .nodes
            .into_iter()
            .find(|n| n.matches(body, since))
            .map(|n| n.database_id))
    }

    /// GitHub keeps one reaction of each content per user on a comment, so
    /// this is retried on any transient failure.
    pub async fn react(
        &self,
        owner: &str,
        repo: &str,
        comment_id: i64,
        content: &str,
    ) -> Result<(), GhError> {
        let path = format!("/repos/{owner}/{repo}/issues/comments/{comment_id}/reactions");
        let _: Option<serde_json::Value> = self
            .write_json(
                "reaction",
                reqwest::Method::POST,
                &path,
                serde_json::json!({ "content": content }),
            )
            .await?;
        Ok(())
    }

    /// Adding a label the PR already has changes nothing, so this is retried
    /// on any transient failure.
    pub async fn add_labels(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
        labels: &[String],
    ) -> Result<(), GhError> {
        let path = format!("/repos/{owner}/{repo}/issues/{number}/labels");
        let _: Option<serde_json::Value> = self
            .write_json(
                "labels",
                reqwest::Method::POST,
                &path,
                serde_json::json!({ "labels": labels }),
            )
            .await?;
        Ok(())
    }

    /// Minimize a comment/review via GraphQL `minimizeComment` mutation.
    /// Minimizing twice leaves it minimized, so this is retried on any
    /// transient failure.
    pub async fn minimize_comment(&self, node_id: &str) -> Result<(), GhError> {
        let q = r#"
            mutation($id: ID!) {
              minimizeComment(input: { subjectId: $id, classifier: OUTDATED }) {
                minimizedComment { isMinimized }
              }
            }
        "#;
        let _: Option<serde_json::Value> = self
            .write_json(
                "minimize",
                reqwest::Method::POST,
                "/graphql",
                serde_json::json!({ "query": q, "variables": { "id": node_id } }),
            )
            .await?;
        Ok(())
    }
}

// --- find_recent_review / find_recent_comment types ---

#[derive(Deserialize)]
struct RecentResponse {
    data: RecentData,
}

#[derive(Deserialize)]
struct RecentData {
    repository: RecentRepository,
}

#[derive(Deserialize)]
struct RecentRepository {
    #[serde(rename = "pullRequest")]
    pull_request: RecentPullRequest,
}

#[derive(Deserialize)]
struct RecentPullRequest {
    #[serde(default)]
    reviews: Option<RecentConnection>,
    #[serde(default)]
    comments: Option<RecentConnection>,
}

#[derive(Deserialize, Default)]
struct RecentConnection {
    nodes: Vec<RecentNode>,
}

#[derive(Deserialize)]
struct RecentNode {
    #[serde(rename = "databaseId")]
    database_id: i64,
    #[serde(default)]
    body: Option<String>,
    #[serde(rename = "createdAt", with = "time::serde::rfc3339")]
    created_at: time::OffsetDateTime,
    #[serde(default)]
    commit: Option<RecentCommit>,
}

#[derive(Deserialize)]
struct RecentCommit {
    oid: String,
}

impl RecentNode {
    fn matches(&self, body: &str, since: time::OffsetDateTime) -> bool {
        self.created_at >= since && self.body.as_deref().map(str::trim) == Some(body.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::client::WriteRetry;
    use std::time::Duration;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn fast_retry() -> WriteRetry {
        WriteRetry {
            deadline: Duration::from_secs(2),
            base_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(50),
            lookup_settle: Duration::from_millis(50),
        }
    }

    fn gh(base: String) -> GitHub {
        GitHub::new(reqwest::Client::new(), "t".into())
            .with_base(base)
            .with_write_retry(fast_retry())
    }

    const REVIEW_BODY: &str = "<!-- barry-dylan:multi-review:barry:v1 -->\n**Barry** - Comment\nok";

    fn review() -> ReviewInput<'static> {
        ReviewInput {
            body: REVIEW_BODY,
            event: "COMMENT",
            comments: &[],
            commit_id: "h1",
        }
    }

    fn now_rfc3339() -> String {
        time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    }

    /// GraphQL's answer listing `nodes` as the PR's recent reviews.
    fn reviews_listing(nodes: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "repository": { "pullRequest": { "reviews": { "nodes": nodes } } } }
        }))
    }

    /// GraphQL's answer listing `nodes` as the PR's recent comments.
    fn comments_listing(nodes: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "repository": { "pullRequest": { "comments": { "nodes": nodes } } } }
        }))
    }

    async fn posts_to(server: &MockServer, p: &str) -> usize {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path() == p)
            .count()
    }

    #[tokio::test]
    async fn a_review_answered_502_that_github_did_record_is_not_posted_twice() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/pulls/1/reviews"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("reviews(last"))
            .respond_with(reviews_listing(serde_json::json!([
                {"databaseId": 77, "body": REVIEW_BODY, "createdAt": now_rfc3339(),
                 "commit": {"oid": "h1"}}
            ])))
            .mount(&server)
            .await;
        let id = gh(server.uri())
            .create_review("o", "r", 1, &review())
            .await
            .unwrap();
        assert_eq!(id, 77);
        assert_eq!(posts_to(&server, "/repos/o/r/pulls/1/reviews").await, 1);
    }

    #[tokio::test]
    async fn a_review_github_shows_only_on_the_second_lookup_is_not_posted_twice() {
        // GitHub answered 502, then took a moment to show the review it had
        // recorded: the first lookup finds nothing.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/pulls/1/reviews"))
            .respond_with(ResponseTemplate::new(502))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/pulls/1/reviews"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 5})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("reviews(last"))
            .respond_with(reviews_listing(serde_json::json!([])))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("reviews(last"))
            .respond_with(reviews_listing(serde_json::json!([
                {"databaseId": 77, "body": REVIEW_BODY, "createdAt": now_rfc3339(),
                 "commit": {"oid": "h1"}}
            ])))
            .mount(&server)
            .await;
        let id = gh(server.uri())
            .create_review("o", "r", 1, &review())
            .await
            .unwrap();
        assert_eq!(id, 77);
        assert_eq!(posts_to(&server, "/repos/o/r/pulls/1/reviews").await, 1);
    }

    #[tokio::test]
    async fn a_lookup_after_a_502_waits_at_least_the_settle_time() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/pulls/1/reviews"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(reviews_listing(serde_json::json!([
                {"databaseId": 77, "body": REVIEW_BODY, "createdAt": now_rfc3339(),
                 "commit": {"oid": "h1"}}
            ])))
            .mount(&server)
            .await;
        let gh = GitHub::new(reqwest::Client::new(), "t".into())
            .with_base(server.uri())
            .with_write_retry(WriteRetry {
                lookup_settle: Duration::from_millis(600),
                ..fast_retry()
            });
        let t = std::time::Instant::now();
        assert_eq!(gh.create_review("o", "r", 1, &review()).await.unwrap(), 77);
        assert!(
            t.elapsed() >= Duration::from_millis(600),
            "{:?}",
            t.elapsed()
        );
    }

    #[tokio::test]
    async fn a_review_answered_502_that_github_did_not_record_is_posted_again() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/pulls/1/reviews"))
            .respond_with(ResponseTemplate::new(502))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/pulls/1/reviews"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 5})))
            .mount(&server)
            .await;
        // The same body on another commit, and an older one on this commit,
        // are not this post.
        let old = (time::OffsetDateTime::now_utc() - time::Duration::hours(1))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("reviews(last"))
            .respond_with(reviews_listing(serde_json::json!([
                {"databaseId": 70, "body": REVIEW_BODY, "createdAt": now_rfc3339(),
                 "commit": {"oid": "h0"}},
                {"databaseId": 71, "body": REVIEW_BODY, "createdAt": old,
                 "commit": {"oid": "h1"}}
            ])))
            .expect(2)
            .mount(&server)
            .await;
        let id = gh(server.uri())
            .create_review("o", "r", 1, &review())
            .await
            .unwrap();
        assert_eq!(id, 5);
        assert_eq!(posts_to(&server, "/repos/o/r/pulls/1/reviews").await, 2);
    }

    #[tokio::test]
    async fn a_review_github_cannot_place_fails_without_a_retry() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/pulls/1/reviews"))
            .respond_with(ResponseTemplate::new(422).set_body_string(
                r#"{"message":"Unprocessable Entity","errors":["Position could not be resolved"]}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(reviews_listing(serde_json::json!([])))
            .expect(0)
            .mount(&server)
            .await;
        let err = gh(server.uri())
            .create_review("o", "r", 1, &review())
            .await
            .unwrap_err();
        assert!(matches!(err, GhError::Api { status: 422, .. }), "{err}");
    }

    #[tokio::test]
    async fn a_comment_answered_502_that_github_did_record_is_not_posted_twice() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/1/comments"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("comments(last"))
            .respond_with(comments_listing(serde_json::json!([
                {"databaseId": 88, "body": "Maximum confers reached for this PR.",
                 "createdAt": now_rfc3339()}
            ])))
            .mount(&server)
            .await;
        let id = gh(server.uri())
            .create_issue_comment("o", "r", 1, "Maximum confers reached for this PR.")
            .await
            .unwrap();
        assert_eq!(id, 88);
        assert_eq!(posts_to(&server, "/repos/o/r/issues/1/comments").await, 1);
    }

    #[tokio::test]
    async fn a_review_refused_a_connection_is_posted_once_the_server_accepts() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Nothing listens on the port for the first 150 ms, so the first
        // attempts are refused before any request is sent. After that, every
        // request is answered 200 with a review id. Any lookup of whether the
        // review landed would get that same answer, fail to parse, and keep
        // the write from succeeding.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let requests = Arc::new(AtomicUsize::new(0));
        let served = requests.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
                .await
                .unwrap();
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                served.fetch_add(1, Ordering::SeqCst);
                let mut buf = vec![0u8; 64 * 1024];
                let _ = sock.read(&mut buf).await;
                let body = r#"{"id":5}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        let id = gh(format!("http://127.0.0.1:{port}"))
            .create_review("o", "r", 1, &review())
            .await
            .unwrap();
        assert_eq!(id, 5);
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

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
    fn a_bot_comment_author_carries_the_bot_suffix_and_a_user_of_the_same_login_does_not() {
        let raw = serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "number": 1, "title": "t", "body": null,
                        "state": "OPEN", "isDraft": false,
                        "additions": 0, "deletions": 0, "changedFiles": 0,
                        "author": { "login": "alice" },
                        "headRefOid": "h", "headRefName": "f",
                        "baseRefOid": "b", "baseRefName": "main",
                        "comments": { "nodes": [
                            {"databaseId": 1, "id": "IC_1", "author": {"__typename": "Bot", "login": "barry-dylan"}, "body": "a"},
                            {"databaseId": 2, "id": "IC_2", "author": {"__typename": "User", "login": "barry-dylan"}, "body": "b"}
                        ]},
                        "reviews": { "nodes": [
                            {"databaseId": 3, "id": "PR_1", "author": {"__typename": "Bot", "login": "barry-dylan"}, "body": "c"}
                        ]}
                    },
                    "config": null
                }
            }
        });
        let parsed: PrCtxResponse = serde_json::from_value(raw).unwrap();
        let ctx = parsed.into_context();
        assert_eq!(ctx.comments[0].author, "barry-dylan[bot]");
        assert_eq!(ctx.comments[1].author, "barry-dylan");
        assert_eq!(ctx.reviews[0].author, "barry-dylan[bot]");
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
