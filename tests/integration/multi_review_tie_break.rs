//! When the judge says Barry and Other Barry disagree, Other Other Barry
//! reviews the same head and its outcome decides `barry/llm-review`.
//!
//! Every identity talks to the one mock server. Other Other Barry's profile
//! names model `oob`, which is how its requests are told apart from the
//! others'.

use async_trait::async_trait;
use barry_dylan::checker::multi_review::identity::Identity;
use barry_dylan::dispatcher::run::{GhFactory, GhFactoryError, MultiGhFactory, run_job};
use barry_dylan::github::client::GitHub;
use barry_dylan::storage::multi_review::RunKey;
use barry_dylan::storage::queue::NewJob;
use std::sync::Arc;
use wiremock::matchers::{body_string_contains, method, path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const OOB_MODEL: &str = r#""model":"oob""#;

fn chat_resp(content: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-x",
        "object": "chat.completion",
        "created": 0,
        "model": "test",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": { "role": "assistant", "content": content }
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    })
}

fn review_json(outcome: &str, summary: &str) -> String {
    format!(r#"{{"outcome":"{outcome}","summary":"{summary}","findings":[]}}"#)
}

/// The PR, its files, and the commenter's permission.
async fn mount_pr(server: &MockServer) {
    mount_pr_at(server, "sha1").await
}

/// [`mount_pr`] with the PR's head at `head`.
async fn mount_pr_at(server: &MockServer, head: &str) {
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(crate::common::graphql_pr_context(
                1,
                "alice",
                head,
                None,
                serde_json::json!([]),
                serde_json::json!([]),
            )),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/repos/o/r/pulls/1/files"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                "filename": "src/lib.rs",
                "status": "modified",
                "additions": 1,
                "deletions": 0,
                "changes": 1,
                "patch": "@@ -1 +1 @@\n+x"
            }])),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/collaborators/alice/permission"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "permission": "write"
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/check-runs"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/pulls/1/reviews"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/1/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 })))
        .mount(server)
        .await;
}

/// Other Other Barry answers with `oob`, the judge with `agree`, and Barry
/// and Other Barry both request changes.
async fn mount_llms(server: &MockServer, agree: bool, oob: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains(OOB_MODEL))
        .respond_with(oob)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("Barry's review"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_resp(&format!(
            r#"{{"agree":{agree},"reason":"they weigh the unwrap differently"}}"#
        ))))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(chat_resp(&review_json("request_changes", "please fix"))),
        )
        .mount(server)
        .await;
}

fn oob_says(outcome: &str) -> ResponseTemplate {
    // Ends in a period, as a model's summary usually does.
    ResponseTemplate::new(200).set_body_json(chat_resp(&review_json(outcome, "oob weighs in.")))
}

/// The default test config with Other Other Barry on model `oob`, after
/// `edit` has had the TOML text.
fn config(server: &MockServer, edit: impl FnOnce(String) -> String) -> barry_dylan::config::Config {
    let text = edit(crate::common::default_config_with_llm_toml(server));
    let mut cfg: barry_dylan::config::Config = toml::from_str(&text).unwrap();
    cfg.llm.get_mut("other_other_barry").unwrap().model = "oob".into();
    cfg
}

fn static_gh(server: &MockServer) -> Arc<dyn MultiGhFactory> {
    Arc::new(crate::common::StaticGh {
        gh: Arc::new(GitHub::new(reqwest::Client::new(), "tok".into()).with_base(server.uri())),
    })
}

fn key() -> RunKey {
    RunKey {
        owner: "o".into(),
        repo: "r".into(),
        pr: 1,
        head_sha: "sha1".into(),
    }
}

async fn requests(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap_or_default()
}

async fn oob_llm_calls(server: &MockServer) -> usize {
    requests(server)
        .await
        .iter()
        .filter(|r| {
            r.url.path() == "/v1/chat/completions"
                && String::from_utf8_lossy(&r.body).contains(OOB_MODEL)
        })
        .count()
}

/// The bodies of every review posted, in order.
async fn review_bodies(server: &MockServer) -> Vec<String> {
    requests(server)
        .await
        .iter()
        .filter(|r| r.method == "POST" && r.url.path() == "/repos/o/r/pulls/1/reviews")
        .map(|r| {
            let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
            v["body"].as_str().unwrap().to_string()
        })
        .collect()
}

/// `barry/llm-review`'s check-runs as posted.
async fn llm_review_checks(server: &MockServer) -> Vec<serde_json::Value> {
    requests(server)
        .await
        .iter()
        .filter(|r| r.method == "POST" && r.url.path() == "/repos/o/r/check-runs")
        .map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).unwrap())
        .filter(|v| v["name"] == "barry/llm-review")
        .collect()
}

async fn the_llm_review_check(server: &MockServer) -> (String, String) {
    let checks = llm_review_checks(server).await;
    assert_eq!(checks.len(), 1, "{checks:?}");
    (
        checks[0]["conclusion"].as_str().unwrap().to_string(),
        checks[0]["output"]["summary"].as_str().unwrap().to_string(),
    )
}

async fn review_pr(
    deps: &barry_dylan::dispatcher::run::JobDeps,
    store: &barry_dylan::storage::Store,
) {
    crate::common::enqueue_opened(store, "o", "r", 1).await;
    let job = store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(deps, &job).await.unwrap();
}

#[tokio::test]
async fn a_disagreement_is_settled_by_other_other_barry_and_the_check_follows_its_outcome() {
    for (outcome, label, conclusion) in [
        ("approve", "Approve", "success"),
        ("comment", "Comment", "neutral"),
        ("request_changes", "RequestChanges", "failure"),
    ] {
        let server = MockServer::start().await;
        mount_pr(&server).await;
        mount_llms(&server, false, oob_says(outcome)).await;
        let (store, deps) =
            crate::common::fixture_with_llm_using(config(&server, |t| t), static_gh(&server)).await;
        review_pr(&deps, &store).await;

        let bodies = review_bodies(&server).await;
        assert_eq!(bodies.len(), 3, "{outcome}: {bodies:#?}");
        assert!(bodies[0].contains("multi-review:barry:"), "{}", bodies[0]);
        assert!(
            bodies[1].contains("multi-review:other_barry:"),
            "{}",
            bodies[1]
        );
        assert!(
            bodies[2].contains(&format!("**Other Other Barry** — {label} (tie-break)")),
            "{}",
            bodies[2]
        );
        assert!(bodies[2].contains("oob weighs in"), "{}", bodies[2]);

        let (got, summary) = the_llm_review_check(&server).await;
        assert_eq!(got, conclusion, "{outcome}: {summary}");
        assert!(summary.contains("Tie-break"), "{summary}");
        assert!(!summary.contains(".."), "{summary}");
        assert!(
            summary.contains("split: Barry RequestChanges, Other Barry RequestChanges"),
            "{summary}"
        );
        assert!(
            summary.contains("they weigh the unwrap differently"),
            "{summary}"
        );

        let st = store.run_state(key()).await.unwrap().unwrap();
        assert!(st.barry_posted && st.other_barry_posted);
        assert!(st.other_other_barry_posted, "{outcome}");
        assert_eq!(st.confers_used, 0, "a tie-break is not a confer");
    }
}

#[tokio::test]
async fn an_agreement_does_not_run_other_other_barry() {
    let server = MockServer::start().await;
    mount_pr(&server).await;
    mount_llms(&server, true, oob_says("approve")).await;
    let (store, deps) =
        crate::common::fixture_with_llm_using(config(&server, |t| t), static_gh(&server)).await;
    review_pr(&deps, &store).await;

    assert_eq!(oob_llm_calls(&server).await, 0);
    assert_eq!(review_bodies(&server).await.len(), 1);
    let (conclusion, _) = the_llm_review_check(&server).await;
    assert_eq!(conclusion, "failure", "Barry's own outcome decides");
}

#[tokio::test]
async fn a_failed_tie_break_leaves_the_check_neutral_with_no_consensus() {
    let server = MockServer::start().await;
    mount_pr(&server).await;
    mount_llms(
        &server,
        false,
        ResponseTemplate::new(400).set_body_string("model not loaded"),
    )
    .await;
    let (store, deps) =
        crate::common::fixture_with_llm_using(config(&server, |t| t), static_gh(&server)).await;
    review_pr(&deps, &store).await;

    assert!(
        oob_llm_calls(&server).await > 0,
        "the tie-break was attempted"
    );
    assert_eq!(review_bodies(&server).await.len(), 2);
    let (conclusion, summary) = the_llm_review_check(&server).await;
    assert_eq!(conclusion, "neutral");
    assert!(
        summary.starts_with("No consensus: they weigh the unwrap differently"),
        "{summary}"
    );
    assert!(summary.contains("tie-break failed"), "{summary}");
    let st = store.run_state(key()).await.unwrap().unwrap();
    assert!(!st.other_other_barry_posted);
}

/// Every identity installed except Other Other Barry.
struct NoOob {
    gh: Arc<GitHub>,
}

#[async_trait]
impl GhFactory for NoOob {
    async fn for_installation(&self, _id: i64) -> anyhow::Result<Arc<GitHub>> {
        Ok(self.gh.clone())
    }
}

#[async_trait]
impl MultiGhFactory for NoOob {
    async fn for_identity(
        &self,
        identity: Identity,
        owner: &str,
        repo: &str,
    ) -> Result<Arc<GitHub>, GhFactoryError> {
        self.preflight_identity(identity, owner, repo).await?;
        Ok(self.gh.clone())
    }

    async fn preflight_identity(
        &self,
        identity: Identity,
        owner: &str,
        repo: &str,
    ) -> Result<(), GhFactoryError> {
        if identity == Identity::OtherOtherBarry {
            return Err(GhFactoryError::NotInstalled {
                identity,
                owner: owner.into(),
                repo: repo.into(),
            });
        }
        Ok(())
    }
}

#[tokio::test]
async fn when_other_other_barry_is_not_installed_a_disagreement_is_neutral_as_before() {
    let server = MockServer::start().await;
    mount_pr(&server).await;
    mount_llms(&server, false, oob_says("approve")).await;
    let factory: Arc<dyn MultiGhFactory> = Arc::new(NoOob {
        gh: Arc::new(GitHub::new(reqwest::Client::new(), "tok".into()).with_base(server.uri())),
    });
    let (store, deps) =
        crate::common::fixture_with_llm_using(config(&server, |t| t), factory).await;
    review_pr(&deps, &store).await;

    assert_eq!(
        oob_llm_calls(&server).await,
        0,
        "no tokens on a review that cannot post"
    );
    assert_eq!(review_bodies(&server).await.len(), 2);
    let (conclusion, summary) = the_llm_review_check(&server).await;
    assert_eq!(conclusion, "neutral");
    assert_eq!(summary, "No consensus: they weigh the unwrap differently");
}

#[tokio::test]
async fn with_the_tie_break_switched_off_a_disagreement_is_neutral_as_before() {
    let server = MockServer::start().await;
    mount_pr(&server).await;
    mount_llms(&server, false, oob_says("approve")).await;
    let cfg = config(&server, |t| {
        t.replace("max_per_pr = 2", "max_per_pr = 2\ntie_break = false")
    });
    let (store, deps) = crate::common::fixture_with_llm_using(cfg, static_gh(&server)).await;
    review_pr(&deps, &store).await;

    assert_eq!(oob_llm_calls(&server).await, 0);
    assert_eq!(review_bodies(&server).await.len(), 2);
    let (conclusion, summary) = the_llm_review_check(&server).await;
    assert_eq!(conclusion, "neutral");
    assert_eq!(summary, "No consensus: they weigh the unwrap differently");
}

#[tokio::test]
async fn a_later_confer_on_the_tie_broken_head_does_not_run_other_other_barry_again() {
    let server = MockServer::start().await;
    mount_pr(&server).await;
    mount_llms(&server, false, oob_says("approve")).await;
    let (store, deps) =
        crate::common::fixture_with_llm_using(config(&server, |t| t), static_gh(&server)).await;
    review_pr(&deps, &store).await;
    let oob_calls = oob_llm_calls(&server).await;
    assert!(oob_calls > 0, "the tie-break ran");

    store
        .enqueue(
            &NewJob {
                installation_id: 1,
                repo_owner: "o".into(),
                repo_name: "r".into(),
                pr_number: 1,
                event_kind: "issue_comment.confer".into(),
                delivery_id: "d-confer".into(),
                actor: Some("alice".into()),
            },
            0,
            0,
        )
        .await
        .unwrap();
    let job = store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(&deps, &job).await.unwrap();

    assert_eq!(oob_llm_calls(&server).await, oob_calls);
    assert_eq!(review_bodies(&server).await.len(), 3);
    let replies: Vec<String> = requests(&server)
        .await
        .iter()
        .filter(|r| r.method == "POST" && r.url.path() == "/repos/o/r/issues/1/comments")
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect();
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert!(
        replies[0].contains("All Barrys have already conferred"),
        "{replies:?}"
    );
    let st = store.run_state(key()).await.unwrap().unwrap();
    assert_eq!(st.confers_used, 0);
}

#[tokio::test]
async fn cancelling_the_review_stops_the_tie_break() {
    let server = MockServer::start().await;
    mount_pr(&server).await;
    // Long enough that a tie-break that ignored the cancel would still be
    // waiting when the job is required to have returned.
    mount_llms(
        &server,
        false,
        oob_says("approve").set_delay(std::time::Duration::from_secs(10)),
    )
    .await;
    let (store, deps) =
        crate::common::fixture_with_llm_using(config(&server, |t| t), static_gh(&server)).await;
    crate::common::enqueue_opened(&store, "o", "r", 1).await;
    let job = store.lease_next(0, 300).await.unwrap().unwrap();
    let deps_bg = deps.clone();
    let handle = tokio::spawn(async move { run_job(&deps_bg, &job).await });

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while oob_llm_calls(&server).await == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the tie-break should have started");
    deps.cancel_registry.cancel("o", "r", 1).await;

    tokio::time::timeout(std::time::Duration::from_secs(3), handle)
        .await
        .expect("the job should stop when cancelled, not wait out Other Other Barry")
        .unwrap()
        .unwrap();

    assert_eq!(review_bodies(&server).await.len(), 2);
    assert!(llm_review_checks(&server).await.is_empty());
    let st = store.run_state(key()).await.unwrap().unwrap();
    assert!(st.barry_posted && st.other_barry_posted);
    assert!(!st.other_other_barry_posted);
}

/// The REST view of PR 1 that the tie-break re-checks the head against.
fn rest_pr(head: &str, state: &str) -> serde_json::Value {
    serde_json::json!({
        "number": 1, "title": "feat: x", "body": "ok",
        "user": { "login": "alice" }, "draft": false, "state": state,
        "head": { "sha": head, "ref": "x" }, "base": { "sha": "base", "ref": "main" },
        "additions": 1, "deletions": 0, "changed_files": 1
    })
}

#[tokio::test]
async fn an_unreachable_other_other_barry_endpoint_is_not_named_in_the_check() {
    let server = MockServer::start().await;
    mount_pr(&server).await;
    mount_llms(&server, false, oob_says("approve")).await;
    let mut cfg = config(&server, |t| t);
    cfg.llm.get_mut("other_other_barry").unwrap().endpoint =
        "http://oob-host.invalid:8080/v1".into();
    let (store, deps) = crate::common::fixture_with_llm_using(cfg, static_gh(&server)).await;
    review_pr(&deps, &store).await;

    let (conclusion, summary) = the_llm_review_check(&server).await;
    assert_eq!(conclusion, "neutral");
    assert_eq!(
        summary,
        "No consensus: they weigh the unwrap differently (tie-break failed: model request failed)"
    );
}

#[tokio::test]
async fn a_huge_error_body_from_other_other_barry_leaves_a_short_summary() {
    let server = MockServer::start().await;
    mount_pr(&server).await;
    let body = format!("line1\nline2 {}", "x".repeat(70_000));
    mount_llms(
        &server,
        false,
        ResponseTemplate::new(400).set_body_string(body),
    )
    .await;
    let (store, deps) =
        crate::common::fixture_with_llm_using(config(&server, |t| t), static_gh(&server)).await;
    review_pr(&deps, &store).await;

    let (conclusion, summary) = the_llm_review_check(&server).await;
    assert_eq!(conclusion, "neutral");
    assert!(summary.len() < 200, "{} bytes", summary.len());
    assert!(!summary.contains('\n'), "{summary}");
}

#[tokio::test]
async fn a_checker_timeout_during_the_tie_break_keeps_no_consensus() {
    let server = MockServer::start().await;
    mount_pr(&server).await;
    mount_llms(
        &server,
        false,
        oob_says("approve").set_delay(std::time::Duration::from_secs(10)),
    )
    .await;
    let cfg = config(&server, |t| {
        t.replace("[dispatcher]", "[dispatcher]\nchecker_timeout_secs = 2")
    });
    let (store, deps) = crate::common::fixture_with_llm_using(cfg, static_gh(&server)).await;
    review_pr(&deps, &store).await;

    assert_eq!(review_bodies(&server).await.len(), 2);
    let (conclusion, summary) = the_llm_review_check(&server).await;
    assert_eq!(conclusion, "neutral");
    assert_eq!(
        summary,
        "No consensus: they weigh the unwrap differently (tie-break failed: timed out)"
    );
}

#[tokio::test]
async fn a_head_that_moved_before_the_tie_break_gets_no_tie_break() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/pulls/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(rest_pr("sha2", "open")))
        .mount(&server)
        .await;
    mount_pr(&server).await;
    mount_llms(&server, false, oob_says("approve")).await;
    let (store, deps) =
        crate::common::fixture_with_llm_using(config(&server, |t| t), static_gh(&server)).await;
    review_pr(&deps, &store).await;

    assert_eq!(
        oob_llm_calls(&server).await,
        0,
        "no tokens on an outdated head"
    );
    assert_eq!(review_bodies(&server).await.len(), 2);
    let st = store.run_state(key()).await.unwrap().unwrap();
    assert!(!st.other_other_barry_posted);
}

/// Answers as Other Other Barry and notes that it has been asked.
struct OobThenPush {
    asked: Arc<std::sync::atomic::AtomicBool>,
}

impl wiremock::Respond for OobThenPush {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        self.asked.store(true, std::sync::atomic::Ordering::SeqCst);
        oob_says("approve")
    }
}

/// The PR's head: `sha1` until Other Other Barry has been asked, `sha2` after.
struct HeadMovesOnce {
    asked: Arc<std::sync::atomic::AtomicBool>,
}

impl wiremock::Respond for HeadMovesOnce {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        let head = if self.asked.load(std::sync::atomic::Ordering::SeqCst) {
            "sha2"
        } else {
            "sha1"
        };
        ResponseTemplate::new(200).set_body_json(rest_pr(head, "open"))
    }
}

#[tokio::test]
async fn a_head_that_moved_during_the_tie_break_gets_no_tie_break_review() {
    let server = MockServer::start().await;
    let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    Mock::given(method("GET"))
        .and(path("/repos/o/r/pulls/1"))
        .respond_with(HeadMovesOnce {
            asked: asked.clone(),
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains(OOB_MODEL))
        .respond_with(OobThenPush { asked })
        .mount(&server)
        .await;
    mount_pr(&server).await;
    mount_llms(&server, false, oob_says("approve")).await;
    let (store, deps) =
        crate::common::fixture_with_llm_using(config(&server, |t| t), static_gh(&server)).await;
    review_pr(&deps, &store).await;

    assert!(
        oob_llm_calls(&server).await > 0,
        "the head was current when it started"
    );
    assert_eq!(review_bodies(&server).await.len(), 2);
    let st = store.run_state(key()).await.unwrap().unwrap();
    assert!(!st.other_other_barry_posted);
}

#[tokio::test]
async fn a_pr_closed_before_the_tie_break_gets_no_tie_break() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/pulls/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(rest_pr("sha1", "closed")))
        .mount(&server)
        .await;
    mount_pr(&server).await;
    mount_llms(&server, false, oob_says("approve")).await;
    let (store, deps) =
        crate::common::fixture_with_llm_using(config(&server, |t| t), static_gh(&server)).await;
    review_pr(&deps, &store).await;

    assert_eq!(oob_llm_calls(&server).await, 0);
    assert_eq!(review_bodies(&server).await.len(), 2);
}

/// Other Other Barry's answer: held for ten seconds while `slow` is set,
/// immediate after.
struct SlowWhileSet {
    slow: Arc<std::sync::atomic::AtomicBool>,
    outcome: &'static str,
}

impl wiremock::Respond for SlowWhileSet {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        let r = oob_says(self.outcome);
        if self.slow.load(std::sync::atomic::Ordering::SeqCst) {
            r.set_delay(std::time::Duration::from_secs(10))
        } else {
            r
        }
    }
}

/// `barry/llm-review`'s check-run: held for ten seconds while `slow` is set.
/// Other check-runs answer at once.
struct CheckSlowWhileSet {
    slow: Arc<std::sync::atomic::AtomicBool>,
}

impl wiremock::Respond for CheckSlowWhileSet {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let r = ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 }));
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
        if body["name"] == "barry/llm-review" && self.slow.load(std::sync::atomic::Ordering::SeqCst)
        {
            r.set_delay(std::time::Duration::from_secs(10))
        } else {
            r
        }
    }
}

fn reviews_by(bodies: &[String], slug: &str) -> usize {
    let marker = format!("multi-review:{slug}:");
    bodies.iter().filter(|b| b.contains(&marker)).count()
}

async fn judge_calls(server: &MockServer) -> usize {
    requests(server)
        .await
        .iter()
        .filter(|r| String::from_utf8_lossy(&r.body).contains("Barry's review"))
        .count()
}

/// Run `job` until `stopped_at` holds, then drop it as a stopping worker
/// does, leaving the job to be leased again.
async fn run_until(
    deps: &Arc<barry_dylan::dispatcher::run::JobDeps>,
    job: &barry_dylan::storage::queue::LeasedJob,
    server: &MockServer,
    stopped_at: impl Fn(&[Request]) -> bool,
) {
    let deps_bg = deps.clone();
    let job_bg = job.clone();
    let handle = tokio::spawn(async move { run_job(&deps_bg, &job_bg).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !stopped_at(&requests(server).await) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the job should have reached the stopping point");
    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
async fn a_job_stopped_during_the_tie_break_resumes_it_without_posting_barry_or_other_barry_again()
{
    let server = MockServer::start().await;
    let slow = Arc::new(std::sync::atomic::AtomicBool::new(true));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains(OOB_MODEL))
        .respond_with(SlowWhileSet {
            slow: slow.clone(),
            outcome: "approve",
        })
        .mount(&server)
        .await;
    mount_pr(&server).await;
    mount_llms(&server, false, oob_says("approve")).await;
    let (store, deps) =
        crate::common::fixture_with_llm_using(config(&server, |t| t), static_gh(&server)).await;
    crate::common::enqueue_opened(&store, "o", "r", 1).await;
    let job = store.lease_next(0, 300).await.unwrap().unwrap();

    run_until(&deps, &job, &server, |rs| {
        rs.iter()
            .any(|r| String::from_utf8_lossy(&r.body).contains(OOB_MODEL))
    })
    .await;
    assert_eq!(
        review_bodies(&server).await.len(),
        2,
        "Barry and Other Barry posted"
    );

    slow.store(false, std::sync::atomic::Ordering::SeqCst);
    run_job(&deps, &job).await.unwrap();

    let bodies = review_bodies(&server).await;
    assert_eq!(reviews_by(&bodies, "barry"), 1, "{bodies:#?}");
    assert_eq!(reviews_by(&bodies, "other_barry"), 1, "{bodies:#?}");
    assert_eq!(reviews_by(&bodies, "other_other_barry"), 1, "{bodies:#?}");
    assert_eq!(
        judge_calls(&server).await,
        1,
        "the verdict is reused, not re-judged"
    );
    let (conclusion, summary) = the_llm_review_check(&server).await;
    assert_eq!(conclusion, "success", "{summary}");
    assert!(
        summary.contains("split: Barry RequestChanges, Other Barry RequestChanges"),
        "{summary}"
    );
}

#[tokio::test]
async fn a_job_stopped_after_the_tie_break_posted_posts_only_the_check() {
    let server = MockServer::start().await;
    let slow = Arc::new(std::sync::atomic::AtomicBool::new(true));
    Mock::given(method("POST"))
        .and(path("/repos/o/r/check-runs"))
        .respond_with(CheckSlowWhileSet { slow: slow.clone() })
        .mount(&server)
        .await;
    mount_pr(&server).await;
    mount_llms(&server, false, oob_says("request_changes")).await;
    let (store, deps) =
        crate::common::fixture_with_llm_using(config(&server, |t| t), static_gh(&server)).await;
    crate::common::enqueue_opened(&store, "o", "r", 1).await;
    let job = store.lease_next(0, 300).await.unwrap().unwrap();

    run_until(&deps, &job, &server, |rs| {
        rs.iter().any(|r| {
            r.url.path() == "/repos/o/r/check-runs"
                && String::from_utf8_lossy(&r.body).contains("barry/llm-review")
        })
    })
    .await;
    assert_eq!(review_bodies(&server).await.len(), 3);
    let llm_calls = requests(&server)
        .await
        .iter()
        .filter(|r| r.url.path() == "/v1/chat/completions")
        .count();

    slow.store(false, std::sync::atomic::Ordering::SeqCst);
    run_job(&deps, &job).await.unwrap();

    assert_eq!(
        review_bodies(&server).await.len(),
        3,
        "nothing posted twice"
    );
    let now_calls = requests(&server)
        .await
        .iter()
        .filter(|r| r.url.path() == "/v1/chat/completions")
        .count();
    assert_eq!(now_calls, llm_calls, "no model is asked again");
    let checks = llm_review_checks(&server).await;
    assert_eq!(
        checks.len(),
        2,
        "the dropped attempt and the one that lands"
    );
    assert_eq!(checks[1]["conclusion"], "failure", "{:?}", checks[1]);
}

#[tokio::test]
async fn a_tie_break_without_time_left_to_post_is_not_posted() {
    let server = MockServer::start().await;
    // Answers inside the review deadline (2.7 s of a 3 s checker timeout),
    // but with less than the time a post may take left before the timeout.
    mount_pr(&server).await;
    mount_llms(
        &server,
        false,
        oob_says("approve").set_delay(std::time::Duration::from_millis(600)),
    )
    .await;
    let cfg = config(&server, |t| {
        t.replace("[dispatcher]", "[dispatcher]\nchecker_timeout_secs = 3")
    });
    let (store, deps) = crate::common::fixture_with_llm_using(cfg, static_gh(&server)).await;
    review_pr(&deps, &store).await;

    assert!(oob_llm_calls(&server).await > 0);
    assert_eq!(review_bodies(&server).await.len(), 2, "no tie-break post");
    let (conclusion, summary) = the_llm_review_check(&server).await;
    assert_eq!(conclusion, "neutral");
    assert_eq!(
        summary,
        "No consensus: they weigh the unwrap differently (tie-break failed: timed out)"
    );
    let st = store.run_state(key()).await.unwrap().unwrap();
    assert!(!st.other_other_barry_posted);
}

#[tokio::test]
async fn a_new_review_job_on_the_same_head_posts_again() {
    // Only a job's own progress stops a repeat post: `/barry review` on an
    // unchanged head is a new job and gets a new review.
    let server = MockServer::start().await;
    mount_pr(&server).await;
    mount_llms(&server, true, oob_says("approve")).await;
    let (store, deps) =
        crate::common::fixture_with_llm_using(config(&server, |t| t), static_gh(&server)).await;
    crate::common::enqueue_opened(&store, "o", "r", 1).await;
    let first = store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(&deps, &first).await.unwrap();
    store.ack(first.id).await.unwrap();
    store
        .enqueue(
            &NewJob {
                installation_id: 1,
                repo_owner: "o".into(),
                repo_name: "r".into(),
                pr_number: 1,
                event_kind: "issue_comment.review".into(),
                delivery_id: "d-review".into(),
                actor: Some("alice".into()),
            },
            0,
            0,
        )
        .await
        .unwrap();
    let command = store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(&deps, &command).await.unwrap();
    store.ack(command.id).await.unwrap();
    // The command queues a review job of its own, due now.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let review = store.lease_next(now + 1, 300).await.unwrap().unwrap();
    run_job(&deps, &review).await.unwrap();

    assert_eq!(reviews_by(&review_bodies(&server).await, "barry"), 2);
}

#[tokio::test]
async fn a_job_handed_back_after_a_push_reviews_the_new_head_afresh() {
    let server = MockServer::start().await;
    mount_pr(&server).await;
    mount_llms(
        &server,
        false,
        oob_says("approve").set_delay(std::time::Duration::from_secs(10)),
    )
    .await;
    let (store, deps) =
        crate::common::fixture_with_llm_using(config(&server, |t| t), static_gh(&server)).await;
    crate::common::enqueue_opened(&store, "o", "r", 1).await;
    let job = store.lease_next(0, 300).await.unwrap().unwrap();
    run_until(&deps, &job, &server, |rs| {
        rs.iter()
            .any(|r| String::from_utf8_lossy(&r.body).contains(OOB_MODEL))
    })
    .await;

    // The head is now sha2, and the reviewers agree on it.
    server.reset().await;
    mount_pr_at(&server, "sha2").await;
    mount_llms(&server, true, oob_says("approve")).await;
    run_job(&deps, &job).await.unwrap();

    assert_eq!(
        judge_calls(&server).await,
        1,
        "a new verdict for the new head"
    );
    let posted: Vec<serde_json::Value> = requests(&server)
        .await
        .iter()
        .filter(|r| r.method == "POST" && r.url.path() == "/repos/o/r/pulls/1/reviews")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert_eq!(posted[0]["commit_id"], "sha2");
}
