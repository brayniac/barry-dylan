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
                actor: None,
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
                actor: None,
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

// ---------------------------------------------------------------------------
// Level 0: the head is re-checked at the last moment before posting
// ---------------------------------------------------------------------------

fn rest_pr(head: &str, state: &str) -> serde_json::Value {
    serde_json::json!({
        "number": 1, "title": "feat: x", "body": "ok",
        "user": { "login": "alice" }, "draft": false, "state": state,
        "head": { "sha": head, "ref": "x" }, "base": { "sha": "base", "ref": "main" },
        "additions": 1, "deletions": 0, "changed_files": 1
    })
}

/// A job whose context (GraphQL) says the head is `sha1`, while the REST
/// lookup at post time says `head`/`state`. Returns how many check-runs were
/// created.
async fn check_runs_posted_when_head_is(head: &str, state: &str) -> usize {
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
        .and(path("/repos/o/r/pulls/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(rest_pr(head, state)))
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
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 1})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/repos/o/r/issues/1/"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 1})))
        .mount(&server)
        .await;

    let store = Store::in_memory().await.unwrap();
    let gh = Arc::new(
        barry_dylan::github::client::GitHub::new(reqwest::Client::new(), "tok".into())
            .with_base(server.uri()),
    );
    let deps = Arc::new(JobDeps {
        store: store.clone(),
        config: Arc::new(crate::common::default_config()),
        pipeline: Arc::new(Pipeline::hygiene_only()),
        gh_factory: Arc::new(crate::common::StaticGh { gh }),
        clients: None,
        personas: None,
        status_tracker: Arc::new(barry_dylan::telemetry::status::StatusTracker::new()),
        cancel_registry: CancelRegistry::new(),
    });
    crate::common::enqueue_opened(&store, "o", "r", 1).await;
    let job = store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(&deps, &job).await.unwrap();

    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.method == "POST" && r.url.path() == "/repos/o/r/check-runs")
        .count()
}

#[tokio::test]
async fn an_outcome_for_a_head_that_moved_is_not_posted() {
    // The push that moved it may not have been delivered (smee keeps no
    // backlog), so the review reaches the post step with a stale outcome.
    assert_eq!(check_runs_posted_when_head_is("sha2", "open").await, 0);
}

#[tokio::test]
async fn an_outcome_for_a_pull_request_since_closed_is_not_posted() {
    assert_eq!(check_runs_posted_when_head_is("sha1", "closed").await, 0);
}

#[tokio::test]
async fn an_outcome_for_the_current_head_is_posted() {
    // The guard must not eat real reviews.
    assert!(check_runs_posted_when_head_is("sha1", "open").await >= 1);
}
