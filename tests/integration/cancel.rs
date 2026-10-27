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

// ---------------------------------------------------------------------------
// A push asks the review in flight to re-check its head against GitHub
// ---------------------------------------------------------------------------

/// Counts its starts, sleeps, then records that it ran to completion. A
/// cancelled review drops it mid-sleep, so `finished` stays false.
struct RecordingSlowChecker {
    started: Arc<std::sync::atomic::AtomicUsize>,
    finished: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl Checker for RecordingSlowChecker {
    fn name(&self) -> &'static str {
        "barry/test.slow"
    }
    fn enabled(&self, _: &RepoConfig) -> bool {
        true
    }
    async fn run(&self, _: &CheckerCtx) -> anyhow::Result<CheckerOutcome> {
        self.started
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        self.finished
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(CheckerOutcome::neutral(self.name(), "done"))
    }
}

struct ReviewInFlight {
    server: MockServer,
    store: Store,
    deps: Arc<JobDeps>,
    registry: CancelRegistry,
    started: Arc<std::sync::atomic::AtomicUsize>,
    finished: Arc<std::sync::atomic::AtomicBool>,
    handle: tokio::task::JoinHandle<anyhow::Result<()>>,
}

/// Start a review whose context (GraphQL) says the head is `sha1`, while a
/// REST lookup of the PR says the head is `current_head`. Returns once the
/// review has registered.
async fn start_review(current_head: &str) -> ReviewInFlight {
    let r = start_review_with(current_head, |started, finished| {
        Arc::new(RecordingSlowChecker { started, finished })
    })
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while r.registry.head_in_flight("o", "r", 1).await.is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the review should have registered");
    r
}

/// Start a review as [`start_review`] does, with the review's only checker
/// built by `checker` from the review's `started` and `finished` records.
/// Returns without waiting for the review to register.
async fn start_review_with(
    current_head: &str,
    checker: impl FnOnce(
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<std::sync::atomic::AtomicBool>,
    ) -> Arc<dyn Checker>,
) -> ReviewInFlight {
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
        .respond_with(ResponseTemplate::new(200).set_body_json(rest_pr(current_head, "open")))
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
    let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut pipeline = Pipeline::hygiene_only();
    pipeline.checkers.clear();
    pipeline
        .checkers
        .push(checker(started.clone(), finished.clone()));
    let registry = CancelRegistry::new();
    let deps = Arc::new(JobDeps {
        store: store.clone(),
        config: Arc::new(crate::common::default_config()),
        pipeline: Arc::new(pipeline),
        gh_factory: Arc::new(crate::common::StaticGh { gh }),
        clients: None,
        personas: None,
        status_tracker: Arc::new(barry_dylan::telemetry::status::StatusTracker::new()),
        cancel_registry: registry.clone(),
    });
    crate::common::enqueue_opened(&store, "o", "r", 1).await;
    let job = store.lease_next(0, 300).await.unwrap().unwrap();
    let deps_bg = deps.clone();
    let handle = tokio::spawn(async move { run_job(&deps_bg, &job).await });
    ReviewInFlight {
        server,
        store,
        deps,
        registry,
        started,
        finished,
        handle,
    }
}

async fn requests_to(server: &MockServer, verb: &str, p: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.method.as_str() == verb && r.url.path() == p)
        .count()
}

#[tokio::test]
async fn a_push_that_moved_the_head_cancels_the_review_in_flight() {
    // GitHub says the head is now sha2, and a push of sha2 arrives.
    let r = start_review("sha2").await;
    assert!(r.registry.push_arrived("o", "r", 1, "sha2").await);
    r.handle.await.unwrap().unwrap();
    assert!(
        !r.finished.load(std::sync::atomic::Ordering::SeqCst),
        "the review of sha1 should have been cancelled mid-run"
    );
    assert_eq!(
        requests_to(&r.server, "POST", "/repos/o/r/check-runs").await,
        0
    );
}

#[tokio::test]
async fn an_old_push_delivered_again_does_not_cancel_the_review_of_the_current_head() {
    // The review is of sha1, GitHub says the head is sha1, and a delivery of
    // an earlier push (sha0) arrives: neither the head under review nor the
    // current head. This also checks that run_job registered the head it is
    // reviewing: had it registered anything else, the re-check would see a
    // moved head and cancel.
    let r = start_review("sha1").await;
    assert!(r.registry.push_arrived("o", "r", 1, "sha0").await);
    r.handle.await.unwrap().unwrap();
    assert!(
        r.finished.load(std::sync::atomic::Ordering::SeqCst),
        "the review of the current head should run to completion"
    );
    assert!(requests_to(&r.server, "POST", "/repos/o/r/check-runs").await >= 1);
    // One lookup from the re-check, one from the pre-post head check.
    assert_eq!(requests_to(&r.server, "GET", "/repos/o/r/pulls/1").await, 2);
}

#[tokio::test]
async fn a_second_job_for_the_head_under_review_does_not_restart_it() {
    // A delivered-again event enqueues a job of its own. When that job runs,
    // it finds the head it would review already under review and stops,
    // rather than cancelling that review and starting over.
    let r = start_review("sha1").await;
    r.store
        .enqueue(
            &NewJob {
                installation_id: 1,
                repo_owner: "o".into(),
                repo_name: "r".into(),
                pr_number: 1,
                event_kind: "pull_request.synchronize".into(),
                delivery_id: "d-again".into(),
                actor: None,
            },
            0,
            0,
        )
        .await
        .unwrap();
    let again = r.store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(&r.deps, &again).await.unwrap();
    r.handle.await.unwrap().unwrap();
    assert_eq!(
        r.started.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the second job should not have started a review of its own"
    );
    assert!(
        r.finished.load(std::sync::atomic::Ordering::SeqCst),
        "the review in flight should run to completion"
    );
}

/// Panics the first time it runs, then runs like [`RecordingSlowChecker`].
struct PanicsOnceChecker {
    started: Arc<std::sync::atomic::AtomicUsize>,
    finished: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl Checker for PanicsOnceChecker {
    fn name(&self) -> &'static str {
        "barry/test.panics-once"
    }
    fn enabled(&self, _: &RepoConfig) -> bool {
        true
    }
    async fn run(&self, _: &CheckerCtx) -> anyhow::Result<CheckerOutcome> {
        if self
            .started
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            panic!("checker panicked");
        }
        self.finished
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(CheckerOutcome::neutral(self.name(), "done"))
    }
}

#[tokio::test]
async fn a_review_that_panicked_does_not_block_a_later_review_of_the_same_head() {
    let r = start_review_with("sha1", |started, finished| {
        Arc::new(PanicsOnceChecker { started, finished })
    })
    .await;
    assert!(
        r.handle.await.unwrap_err().is_panic(),
        "the first review should have panicked"
    );
    crate::common::enqueue_opened(&r.store, "o", "r", 1).await;
    let again = r.store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(&r.deps, &again).await.unwrap();
    assert_eq!(r.started.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(r.finished.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn an_older_review_finishing_does_not_unregister_the_newer_one() {
    // A review of sha2 registers while the review of sha1 is in flight,
    // which cancels the sha1 review. When that review returns, the sha2
    // review must still be registered, so a later push still reaches it.
    let r = start_review("sha1").await;
    let newer = r.registry.register("o", "r", 1, "sha2").await.unwrap();
    r.handle.await.unwrap().unwrap();
    assert!(
        r.registry.push_arrived("o", "r", 1, "sha3").await,
        "the review of sha2 should still be registered"
    );
    assert!(!newer.token.is_cancelled());
}

#[test]
fn a_job_skipped_for_a_review_in_flight_is_counted() {
    // The skip is otherwise visible only in an info log.
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // A current-thread runtime keeps every task on this thread, where the
    // local recorder is installed.
    metrics::with_local_recorder(&recorder, || {
        rt.block_on(async {
            let r = start_review("sha1").await;
            crate::common::enqueue_opened(&r.store, "o", "r", 1).await;
            let again = r.store.lease_next(0, 300).await.unwrap().unwrap();
            run_job(&r.deps, &again).await.unwrap();
            r.handle.await.unwrap().unwrap();
        })
    });
    let rendered = handle.render();
    assert!(
        rendered
            .lines()
            .any(|l| l == r#"barry_review_skipped_total{reason="in_flight"} 1"#),
        "{rendered}"
    );
}
