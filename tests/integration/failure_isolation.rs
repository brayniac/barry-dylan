use async_trait::async_trait;
use barry_dylan::checker::{Checker, CheckerCtx, CheckerOutcome};
use barry_dylan::config::repo::RepoConfig;
use barry_dylan::dispatcher::run::{JobDeps, Pipeline, run_job};
use std::sync::Arc;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct AlwaysFail;
#[async_trait]
impl Checker for AlwaysFail {
    fn name(&self) -> &'static str {
        "barry/test.fail"
    }
    fn enabled(&self, _: &RepoConfig) -> bool {
        true
    }
    async fn run(&self, _: &CheckerCtx) -> anyhow::Result<CheckerOutcome> {
        anyhow::bail!("boom")
    }
}

#[tokio::test]
async fn one_checker_error_does_not_block_others() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(crate::common::graphql_pr_context(
                1,
                "alice",
                "sha1",
                None,
                serde_json::json!([]),
                serde_json::json!([]),
            )),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/repos/o/r/pulls/1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/collaborators/alice/permission"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "permission": "write"
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/repos/o/r/check-runs"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 })))
        // expect at least 2: one neutral for AlwaysFail, plus title (success) and others.
        .expect(2..)
        .mount(&server)
        .await;

    let store = barry_dylan::storage::Store::in_memory().await.unwrap();
    let gh = std::sync::Arc::new(
        barry_dylan::github::client::GitHub::new(reqwest::Client::new(), "t".into())
            .with_base(server.uri()),
    );
    let mut pipeline = Pipeline::hygiene_only();
    pipeline.checkers.push(Arc::new(AlwaysFail));
    let deps = Arc::new(JobDeps {
        store: store.clone(),
        config: Arc::new(crate::common::default_config()),
        pipeline: Arc::new(pipeline),
        gh_factory: Arc::new(crate::common::StaticGh { gh }),
        clients: None,
        personas: None,
        status_tracker: Arc::new(barry_dylan::telemetry::status::StatusTracker::new()),
    });

    crate::common::enqueue_opened(&store, "o", "r", 1).await;
    let job = store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(&deps, &job).await.unwrap();
}
