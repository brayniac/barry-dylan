use async_trait::async_trait;
use barry_dylan::checker::multi_review::identity::Identity;
use barry_dylan::dispatcher::run::{GhFactory, JobDeps, MultiGhFactory, Pipeline};
use barry_dylan::github::client::GitHub;
use barry_dylan::storage::Store;
use barry_dylan::storage::queue::NewJob;
use std::sync::Arc;
use wiremock::MockServer;
use wiremock::matchers;

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

#[async_trait]
impl MultiGhFactory for StaticGh {
    async fn for_identity(&self, _identity: Identity, _inst: i64) -> anyhow::Result<Arc<GitHub>> {
        // All identities share one mock GitHub in tests.
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

        [github.barry]
        app_id = 1
        private_key_path = "/dev/null"
        webhook_secret_env = "X"

        [github.other_barry]
        app_id = 2
        private_key_path = "/dev/null"

        [github.other_other_barry]
        app_id = 3
        private_key_path = "/dev/null"

        [storage]
        sqlite_path = "/tmp/x.db"

        [dispatcher]

        [llm.barry]
        provider = "anthropic"
        endpoint = "https://api.anthropic.com"
        model = "m"

        [llm.other_barry]
        provider = "openai"
        endpoint = "http://localhost:1/v1"
        model = "m"

        [llm.other_other_barry]
        provider = "openai"
        endpoint = "https://api.openai.com/v1"
        model = "m"

        [llm.judge]
        provider = "anthropic"
        endpoint = "https://api.anthropic.com"
        model = "m"
    "#;
    toml::from_str(toml).unwrap()
}

#[allow(dead_code)]
pub async fn mock_openai_chat(server: &MockServer, response_text: &str) {
    let resp = serde_json::json!({
        "id": "chatcmpl-x",
        "object": "chat.completion",
        "created": 0,
        "model": "test",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": { "role": "assistant", "content": response_text }
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    });
    wiremock::Mock::given(matchers::method("POST"))
        .and(matchers::path("/v1/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(resp))
        .mount(server)
        .await;
}

pub async fn fixture_with_llm(server: &MockServer) -> (Store, Arc<JobDeps>) {
    use barry_dylan::checker::multi_review::clients::IdentityClients;
    use barry_dylan::checker::multi_review::{MultiReviewChecker, persona};

    let store = Store::in_memory().await.unwrap();
    let gh = Arc::new(GitHub::new(reqwest::Client::new(), "tok".into()).with_base(server.uri()));
    let cfg = Arc::new(default_config_with_llm(server));

    let clients = Arc::new(IdentityClients {
        barry: barry_dylan::llm::factory::build(&cfg.llm["barry"], reqwest::Client::new()).unwrap(),
        other_barry: barry_dylan::llm::factory::build(
            &cfg.llm["other_barry"],
            reqwest::Client::new(),
        )
        .unwrap(),
        other_other_barry: barry_dylan::llm::factory::build(
            &cfg.llm["other_other_barry"],
            reqwest::Client::new(),
        )
        .unwrap(),
        judge: barry_dylan::llm::factory::build(&cfg.llm["judge"], reqwest::Client::new()).unwrap(),
        barry_max_tokens: 1024,
        other_barry_max_tokens: 1024,
        other_other_barry_max_tokens: 1024,
        judge_max_tokens: 256,
    });
    let personas = Arc::new(persona::resolve(&persona::PersonaOverrides::default()).unwrap());
    let factory: Arc<dyn MultiGhFactory> = Arc::new(StaticGh { gh: gh.clone() });

    let mut pipeline = Pipeline::hygiene_only();
    pipeline.checkers.push(Arc::new(MultiReviewChecker {
        clients,
        personas,
        gh_factory: factory.clone(),
    }));

    let deps = Arc::new(JobDeps {
        store: store.clone(),
        config: cfg,
        pipeline: Arc::new(pipeline),
        gh_factory: factory,
    });
    (store, deps)
}

pub fn default_config_with_llm(server: &MockServer) -> barry_dylan::config::Config {
    let toml = format!(
        r#"
        [server]
        listen = "0.0.0.0:0"
        [github.barry]
        app_id = 1
        private_key_path = "/dev/null"
        webhook_secret_env = "X"
        [github.other_barry]
        app_id = 2
        private_key_path = "/dev/null"
        [github.other_other_barry]
        app_id = 3
        private_key_path = "/dev/null"
        [storage]
        sqlite_path = "/tmp/x.db"
        [dispatcher]
        [llm.barry]
        provider = "openai"
        endpoint = "{base}/v1"
        model = "x"
        [llm.other_barry]
        provider = "openai"
        endpoint = "{base}/v1"
        model = "x"
        [llm.other_other_barry]
        provider = "openai"
        endpoint = "{base}/v1"
        model = "x"
        [llm.judge]
        provider = "openai"
        endpoint = "{base}/v1"
        model = "x"
        [confer]
        allowed = ["author", "write", "admin"]
        max_per_pr = 2
    "#,
        base = server.uri()
    );
    toml::from_str(&toml).unwrap()
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
