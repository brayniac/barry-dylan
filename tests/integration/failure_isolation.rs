use async_trait::async_trait;
use barry_dylan::checker::{Checker, CheckerCtx, CheckerOutcome};
use barry_dylan::config::repo::RepoConfig;
use barry_dylan::dispatcher::run::{run_job, JobDeps, Pipeline};
use std::sync::Arc;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct AlwaysFail;
#[async_trait]
impl Checker for AlwaysFail {
    fn name(&self) -> &'static str { "barry/test.fail" }
    fn enabled(&self, _: &RepoConfig) -> bool { true }
    async fn run(&self, _: &CheckerCtx) -> anyhow::Result<CheckerOutcome> {
        anyhow::bail!("boom")
    }
}

#[tokio::test]
async fn one_checker_error_does_not_block_others() {
    let server = MockServer::start().await;

    Mock::given(method("GET")).and(path("/repos/o/r/pulls/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "number": 1, "title": "feat: add x", "body": "Long enough body to pass checks.",
            "user": { "login": "alice" }, "draft": false, "state": "open",
            "head": { "sha": "sha1", "ref": "feat" },
            "base": { "sha": "sha0", "ref": "main" },
            "additions": 1, "deletions": 0, "changed_files": 1
        }))).mount(&server).await;
    Mock::given(method("GET")).and(path_regex(r"^/repos/o/r/pulls/1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/repos/o/r/collaborators/alice/permission"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "permission": "write"
        }))).mount(&server).await;
    Mock::given(method("GET")).and(path("/repos/o/r/contents/.barry.toml"))
        .respond_with(ResponseTemplate::new(404)).mount(&server).await;
    Mock::given(method("GET")).and(path("/repos/o/r/issues/1/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/repos/o/r/pulls/1/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server).await;

    Mock::given(method("POST")).and(path("/repos/o/r/check-runs"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 })))
        // expect at least 2: one neutral for AlwaysFail, plus title (success) and others.
        .expect(2..)
        .mount(&server).await;

    let store = barry_dylan::storage::Store::in_memory().await.unwrap();
    let gh = std::sync::Arc::new(barry_dylan::github::client::GitHub::new(
        reqwest::Client::new(), "t".into()).with_base(server.uri()));
    let mut pipeline = Pipeline::hygiene_only();
    pipeline.checkers.push(Arc::new(AlwaysFail));
    let deps = Arc::new(JobDeps {
        store: store.clone(),
        config: Arc::new(crate::common::default_config()),
        pipeline: Arc::new(pipeline),
        gh_factory: Arc::new(crate::common::StaticGh { gh }),
    });

    crate::common::enqueue_opened(&store, "o", "r", 1).await;
    let job = store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(&deps, &job).await.unwrap();
}
