use async_trait::async_trait;
use barry_dylan::checker::{Checker, CheckerCtx, CheckerOutcome};
use barry_dylan::config::repo::RepoConfig;
use barry_dylan::dispatcher::cancel::CancelRegistry;
use barry_dylan::dispatcher::run::{JobDeps, Pipeline, run_job};
use barry_dylan::storage::Store;
use barry_dylan::storage::queue::NewJob;
use std::sync::Arc;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Test 1: Level 2 — close event purges pending review jobs from the queue
// ---------------------------------------------------------------------------

#[tokio::test]
async fn close_event_purges_queued_review_jobs() {
    let store = Store::in_memory().await.unwrap();

    // Enqueue a review job for PR #1.
    store
        .enqueue(
            &NewJob {
                installation_id: 1,
                repo_owner: "o".into(),
                repo_name: "r".into(),
                pr_number: 1,
                event_kind: "pull_request.opened".into(),
                delivery_id: "d-review".into(),
            },
            0,
            0,
        )
        .await
        .unwrap();

    // Verify it's there.
    let leased = store.lease_next(0, 300).await.unwrap();
    assert!(leased.is_some(), "review job should be in queue");
    // Put it back by letting the lease expire — simpler: just use a fresh store.
    drop(leased);

    // Use a separate store to avoid the leased state; re-check via cancel_pr_jobs directly.
    let store2 = Store::in_memory().await.unwrap();
    store2
        .enqueue(
            &NewJob {
                installation_id: 1,
                repo_owner: "o".into(),
                repo_name: "r".into(),
                pr_number: 2,
                event_kind: "pull_request.opened".into(),
                delivery_id: "d-review2".into(),
            },
            0,
            0,
        )
        .await
        .unwrap();

    // cancel_pr_jobs should delete the pending job.
    store2.cancel_pr_jobs("o", "r", 2).await.unwrap();

    // No job should remain.
    let after = store2.lease_next(0, 300).await.unwrap();
    assert!(after.is_none(), "review job should have been purged");
}

// ---------------------------------------------------------------------------
// Test 2: Level 3 — in-flight cancellation prevents check-run posts
// ---------------------------------------------------------------------------

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
        // Sleep long enough that the cancel token fires before we return.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        Ok(CheckerOutcome::neutral(self.name(), "should not post"))
    }
}

#[tokio::test]
async fn inflight_cancellation_prevents_posting() {
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

    // Check-runs and comments must NOT be called.
    Mock::given(method("POST"))
        .and(path("/repos/o/r/check-runs"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let store = Store::in_memory().await.unwrap();
    let gh = Arc::new(
        barry_dylan::github::client::GitHub::new(reqwest::Client::new(), "tok".into())
            .with_base(server.uri()),
    );
    let cancel_registry = CancelRegistry::new();
    let mut pipeline = Pipeline::hygiene_only();
    pipeline.checkers.clear(); // only the slow checker
    pipeline.checkers.push(Arc::new(SlowChecker));

    let deps = Arc::new(JobDeps {
        store: store.clone(),
        config: Arc::new(crate::common::default_config()),
        pipeline: Arc::new(pipeline),
        gh_factory: Arc::new(crate::common::StaticGh { gh }),
        clients: None,
        personas: None,
        status_tracker: Arc::new(barry_dylan::telemetry::status::StatusTracker::new()),
        cancel_registry: cancel_registry.clone(),
    });

    crate::common::enqueue_opened(&store, "o", "r", 1).await;
    let job = store.lease_next(0, 300).await.unwrap().unwrap();

    // Run the job in the background. The SlowChecker sleeps 500ms.
    let deps_bg = deps.clone();
    let job_arc = Arc::new(job);
    let job_bg = job_arc.clone();
    let handle = tokio::spawn(async move { run_job(&deps_bg, &job_bg).await });

    // Wait just long enough for run_job to reach the checker loop and register the
    // cancel token (all mock calls are instant), then fire cancellation.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    cancel_registry.cancel("o", "r", 1).await;

    // The job should complete (not hang) and return Ok even though cancelled.
    handle.await.unwrap().unwrap();

    // wiremock verifies expect(0) on teardown when MockServer is dropped.
}
