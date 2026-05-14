use async_trait::async_trait;
use barry_dylan::dispatcher::run::{GhFactory, JobDeps, Pipeline};
use barry_dylan::github::client::GitHub;
use barry_dylan::storage::Store;
use barry_dylan::storage::queue::NewJob;
use std::sync::Arc;
use wiremock::MockServer;

/// Build a GraphQL response body for `fetch_pr_context`. Comments/reviews arrays
/// hold zero or more `{databaseId,id,author:{login},body}` nodes.
pub fn graphql_pr_context(
    pr_number: i64,
    author_login: &str,
    head_sha: &str,
    config_text: Option<&str>,
    comments: serde_json::Value,
    reviews: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "data": {
            "repository": {
                "pullRequest": {
                    "number": pr_number,
                    "title": "feat: add x",
                    "body": "Long enough body to pass checks.",
                    "state": "OPEN",
                    "isDraft": false,
                    "additions": 1,
                    "deletions": 0,
                    "changedFiles": 1,
                    "author": { "login": author_login },
                    "headRefOid": head_sha,
                    "headRefName": "feat",
                    "baseRefOid": "sha0",
                    "baseRefName": "main",
                    "comments": { "nodes": comments },
                    "reviews": { "nodes": reviews },
                },
                "config": config_text.map(|t| serde_json::json!({ "text": t })),
            }
        }
    })
}

pub struct StaticGh {
    pub gh: Arc<GitHub>,
}
#[async_trait]
impl GhFactory for StaticGh {
    async fn for_installation(&self, _id: i64) -> anyhow::Result<Arc<GitHub>> {
        Ok(self.gh.clone())
    }
}

pub async fn fixture(server: &MockServer) -> (Store, Arc<JobDeps>) {
    let store = Store::in_memory().await.unwrap();
    let gh = Arc::new(GitHub::new(reqwest::Client::new(), "tok".into()).with_base(server.uri()));
    let pipeline = Arc::new(Pipeline::hygiene_only());
    let cfg = Arc::new(default_config());
    let deps = Arc::new(JobDeps {
        store: store.clone(),
        config: cfg,
        pipeline,
        gh_factory: Arc::new(StaticGh { gh }),
    });
    (store, deps)
}

pub fn default_config() -> barry_dylan::config::Config {
    let toml = r#"
        [server]
        listen = "0.0.0.0:0"
        [github]
        app_id = 1
        private_key_path = "/dev/null"
        webhook_secret_env = "X"
        [storage]
        sqlite_path = "/tmp/x.db"
        [dispatcher]
        [llm.default]
        provider = "anthropic"
        endpoint = "https://api.anthropic.com"
        model = "m"
    "#;
    toml::from_str(toml).unwrap()
}

pub fn enqueue_opened<'a>(
    store: &'a Store,
    owner: &'a str,
    repo: &'a str,
    pr: i64,
) -> impl std::future::Future<Output = ()> + 'a {
    let job = NewJob {
        installation_id: 1,
        repo_owner: owner.into(),
        repo_name: repo.into(),
        pr_number: pr,
        event_kind: "pull_request.opened".into(),
        delivery_id: "d1".into(),
    };
    async move {
        store.enqueue(&job, 0, 0).await.unwrap();
    }
}
