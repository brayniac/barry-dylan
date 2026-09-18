//! A restart hands the job in hand back instead of finishing it (#39).

use async_trait::async_trait;
use barry_dylan::checker::{Checker, CheckerCtx, CheckerOutcome};
use barry_dylan::config::repo::RepoConfig;
use barry_dylan::dispatcher::run::{JobDeps, Pipeline};
use barry_dylan::dispatcher::worker::run_worker;
use barry_dylan::storage::Store;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Long enough that shutdown arrives mid-checker.
struct SlowChecker;

#[async_trait]
impl Checker for SlowChecker {
    fn name(&self) -> &'static str {
        "barry/test.slow"
    }
    fn enabled(&self, _: &RepoConfig) -> bool {
        true
    }
    async fn run(&self, _: &CheckerCtx) -> anyhow::Result<CheckerOutcome> {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        Ok(CheckerOutcome::neutral(
            self.name(),
            "should never be reached",
        ))
    }
}

#[tokio::test]
async fn shutdown_mid_job_hands_the_job_back_at_once() {
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

    let store = Store::in_memory().await.unwrap();
    let gh = Arc::new(
        barry_dylan::github::client::GitHub::new(reqwest::Client::new(), "tok".into())
            .with_base(server.uri()),
    );
    let mut pipeline = Pipeline::hygiene_only();
    pipeline.checkers.clear();
    pipeline.checkers.push(Arc::new(SlowChecker));
    let deps = Arc::new(JobDeps {
        store: store.clone(),
        config: Arc::new(crate::common::default_config()),
        pipeline: Arc::new(pipeline),
        gh_factory: Arc::new(crate::common::StaticGh { gh }),
        clients: None,
        personas: None,
        status_tracker: Arc::new(barry_dylan::telemetry::status::StatusTracker::new()),
        cancel_registry: barry_dylan::dispatcher::cancel::CancelRegistry::new(),
    });
    crate::common::enqueue_opened(&store, "o", "r", 1).await;

    let shutdown = CancellationToken::new();
    let worker = tokio::spawn(run_worker(deps.clone(), 300, shutdown.clone()));

    // Let the worker lease the job and get into the slow checker.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(
        store.count_rows("jobs").await.unwrap(),
        1,
        "the job is leased, not gone"
    );
    let started = std::time::Instant::now();
    shutdown.cancel();

    // The worker must return well inside the checker's five seconds: it does
    // not finish the job in hand, it hands it back.
    tokio::time::timeout(std::time::Duration::from_secs(2), worker)
        .await
        .expect("worker stopped promptly")
        .unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(2));

    // And the job is leasable right now by the next process, not after the
    // lease expires, and without an attempt spent.
    let now = barry_dylan::util::now_ts();
    let again = store
        .lease_next(now, 300)
        .await
        .unwrap()
        .expect("the job was handed back for the next process");
    assert_eq!(again.pr_number, 1);
    assert_eq!(again.attempts, 0);
}
