use crate::storage::Store;
use crate::storage::queue::NewJob;
use crate::webhook::event::{InboundEvent, parse};
use crate::webhook::verify;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use metrics_exporter_prometheus::PrometheusHandle;
use std::sync::Arc;

/// Which repositories barry acts on.
///
/// Built once at startup so the hot path is a hash lookup, and lowercased
/// because GitHub treats `Brayniac/Slipway` and `brayniac/slipway` as the same
/// repository while a `HashSet<String>` does not.
#[derive(Debug, Default, Clone)]
pub struct RepoFilter {
    allowed: Option<std::collections::HashSet<String>>,
}

impl RepoFilter {
    pub fn new(configured: Option<&Vec<String>>) -> Self {
        Self {
            allowed: configured.map(|repos| repos.iter().map(|r| r.to_ascii_lowercase()).collect()),
        }
    }

    pub fn allows(&self, owner: &str, repo: &str) -> bool {
        match &self.allowed {
            None => true,
            Some(set) => set.contains(&format!("{owner}/{repo}").to_ascii_lowercase()),
        }
    }

    /// How many repositories are listed, or `None` for "every one".
    pub fn configured_count(&self) -> Option<usize> {
        self.allowed.as_ref().map(|s| s.len())
    }
}

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub webhook_secret: Arc<Vec<u8>>,
    pub metrics: PrometheusHandle,
    pub debounce_secs: u64,
    /// Repositories barry acts on. Checked before a delivery becomes a job,
    /// so a repository barry is installed on but not configured for costs one
    /// string compare rather than a worker and a GPU host.
    pub repos: Arc<RepoFilter>,
    /// Present when events arrive through [`crate::relay`] rather than
    /// directly. Reported by `/healthz`: smee keeps no backlog, so a relay
    /// that has quietly stopped looks exactly like a repository where nobody
    /// opened a pull request.
    pub relay: Option<Arc<crate::relay::Status>>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/webhook", post(webhook))
        .with_state(state)
}

async fn healthz(State(s): State<AppState>) -> impl IntoResponse {
    if s.store.query_raw("SELECT 1").await.is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "db unavailable".to_string(),
        );
    }
    let Some(relay) = &s.relay else {
        return (StatusCode::OK, "ok".to_string());
    };
    // A disconnected relay is not reported as unhealthy: it reconnects on its
    // own, and a health check that flaps every time smee drops a long-lived
    // connection is a health check nobody reads. The state is reported so a
    // human or `infra diff` can see a relay that has been down for hours.
    let last = match relay.last_event_unix() {
        Some(t) => format!("{}s ago", crate::util::now_ts().saturating_sub(t)),
        None => "never".to_string(),
    };
    (
        StatusCode::OK,
        format!(
            "ok\nrelay: {}\nrelay_events: {}\nrelay_last_event: {}\n",
            if relay.connected() {
                "connected"
            } else {
                "disconnected"
            },
            relay.events(),
            last
        ),
    )
}

async fn metrics(State(s): State<AppState>) -> impl IntoResponse {
    (StatusCode::OK, s.metrics.render())
}

/// The repository a delivery is about, when it is about one.
fn repo_of(parsed: &InboundEvent) -> Option<(&str, &str)> {
    match parsed {
        InboundEvent::PullRequest(e) => Some((&e.repository.owner.login, &e.repository.name)),
        InboundEvent::IssueComment(e) => Some((&e.repository.owner.login, &e.repository.name)),
        InboundEvent::Ignored(_) => None,
    }
}

async fn webhook(State(s): State<AppState>, headers: HeaderMap, body: Bytes) -> impl IntoResponse {
    metrics::counter!("barry_webhook_received_total").increment(1);

    let sig = headers
        .get("X-Hub-Signature-256")
        .and_then(|v| v.to_str().ok());
    if let Err(e) = verify::verify(&s.webhook_secret, &body, sig) {
        tracing::warn!(?e, "webhook signature verification failed");
        metrics::counter!("barry_webhook_rejected_total", "reason" => "signature").increment(1);
        return (StatusCode::UNAUTHORIZED, "bad signature");
    }
    let evt = headers.get("X-GitHub-Event").and_then(|v| v.to_str().ok());
    let delivery = headers
        .get("X-GitHub-Delivery")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    let parsed = match parse(evt, &body) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(?e, event = evt, delivery_id = %delivery, "webhook parse failed");
            metrics::counter!("barry_webhook_rejected_total", "reason" => "parse").increment(1);
            return (StatusCode::BAD_REQUEST, "bad payload");
        }
    };
    if let Some((owner, repo)) = repo_of(&parsed)
        && !s.repos.allows(owner, repo)
    {
        // Not an error and not a signature problem: barry is installed on this
        // repository and deliberately does not act on it. Counted so that "why
        // did barry ignore my pull request" has an answer.
        tracing::debug!(
            event = evt,
            delivery_id = %delivery,
            repo = %format!("{owner}/{repo}"),
            "not a configured repository; ignoring"
        );
        metrics::counter!("barry_webhook_rejected_total", "reason" => "repo").increment(1);
        return (StatusCode::OK, "not a configured repository");
    }

    tracing::info!(event = evt, delivery_id = %delivery, "webhook received");

    let now = crate::util::now_ts();
    let debounce = s.debounce_secs as i64;

    let to_enqueue: Option<(NewJob, i64)> = match parsed {
        InboundEvent::PullRequest(e) if pull_request_action_is_actionable(&e.action) => {
            let pr = e.number;
            let owner = e.repository.owner.login.clone();
            let repo = e.repository.name.clone();
            let head_sha = e.pull_request.head.sha.clone();

            let span = tracing::info_span!(
                "webhook.pr",
                delivery_id = %delivery,
                owner = %owner,
                repo = %repo,
                pr = pr,
                head_sha = %head_sha,
                event_kind = %format!("pull_request.{}", e.action)
            );
            let _enter = span.enter();

            tracing::info!(
                owner = %owner,
                repo = %repo,
                pr = pr,
                action = %e.action,
                "processing pull request webhook"
            );

            // Populate Barry's installation cache so downstream factory calls
            // for (Barry, owner) are a free DB lookup.
            let _ = s
                .store
                .put_installation("barry", &owner, Some(e.installation.id), now)
                .await;

            Some((
                NewJob {
                    installation_id: e.installation.id,
                    repo_owner: owner,
                    repo_name: repo,
                    pr_number: pr,
                    event_kind: format!("pull_request.{}", e.action),
                    delivery_id: delivery.clone(),
                    actor: None,
                },
                if e.action == "synchronize" {
                    now + debounce
                } else {
                    now
                },
            ))
        }
        InboundEvent::IssueComment(e)
            if e.action == "created"
                && e.issue.pull_request.is_some()
                && e.comment.body.starts_with("/barry") =>
        {
            let pr = e.issue.number;
            let owner = e.repository.owner.login.clone();
            let repo = e.repository.name.clone();

            let span = tracing::info_span!(
                "webhook.command",
                delivery_id = %delivery,
                owner = %owner,
                repo = %repo,
                pr = pr,
                command = %short_command(&e.comment.body)
            );
            let _enter = span.enter();

            tracing::info!(
                owner = %owner,
                repo = %repo,
                pr = pr,
                "processing /barry command"
            );

            // Populate Barry's installation cache so downstream factory calls
            // for (Barry, owner) are a free DB lookup.
            let _ = s
                .store
                .put_installation("barry", &owner, Some(e.installation.id), now)
                .await;

            Some((
                NewJob {
                    installation_id: e.installation.id,
                    repo_owner: owner,
                    repo_name: repo,
                    pr_number: pr,
                    event_kind: format!("issue_comment.{}", short_command(&e.comment.body)),
                    delivery_id: delivery.clone(),
                    actor: Some(e.sender.login.clone()),
                },
                now,
            ))
        }
        InboundEvent::PullRequest(e) if e.action == "closed" => {
            let owner = e.repository.owner.login.clone();
            let repo = e.repository.name.clone();
            tracing::info!(
                owner = %owner,
                repo = %repo,
                pr = e.number,
                "PR closed webhook received"
            );

            // Populate Barry's installation cache so downstream factory calls
            // for (Barry, owner) are a free DB lookup.
            let _ = s
                .store
                .put_installation("barry", &owner, Some(e.installation.id), now)
                .await;

            Some((
                NewJob {
                    installation_id: e.installation.id,
                    repo_owner: owner,
                    repo_name: repo,
                    pr_number: e.number,
                    event_kind: "pull_request.closed".into(),
                    delivery_id: delivery.clone(),
                    actor: None,
                },
                now,
            ))
        }
        _ => {
            tracing::debug!(event = evt, delivery_id = %delivery, "event dropped (not actionable)");
            None
        }
    };

    if let Some((job, run_after)) = to_enqueue {
        let owner = &job.repo_owner;
        let repo = &job.repo_name;
        let pr = job.pr_number;
        let kind = &job.event_kind;
        if let Err(e) = s.store.enqueue(&job, now, run_after).await {
            tracing::error!(
                ?e,
                %owner,
                %repo,
                pr,
                event_kind = %kind,
                "enqueue failed"
            );
            metrics::counter!("barry_webhook_rejected_total", "reason" => "enqueue").increment(1);
            return (StatusCode::INTERNAL_SERVER_ERROR, "enqueue failed");
        }
        metrics::counter!("barry_job_enqueued_total").increment(1);
        tracing::info!(
            %owner,
            %repo,
            pr,
            event_kind = %kind,
            delivery_id = %delivery,
            run_after_in_secs = run_after - now,
            "job enqueued"
        );
    }
    (StatusCode::OK, "ok")
}

fn pull_request_action_is_actionable(a: &str) -> bool {
    matches!(
        a,
        "opened" | "synchronize" | "reopened" | "ready_for_review"
    )
}

fn short_command(body: &str) -> &'static str {
    let first = body.split_whitespace().nth(1).unwrap_or("");
    match first {
        "approve" => "approve",
        "review" => "review",
        "confer" => "confer",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use hmac::Mac;
    use sha2::Sha256;
    use tower::ServiceExt;

    fn sign(secret: &[u8], body: &[u8]) -> String {
        let mut m = <hmac::Hmac<Sha256>>::new_from_slice(secret).unwrap();
        m.update(body);
        let bytes = m.finalize().into_bytes();
        let mut s = String::from("sha256=");
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    /// A router that acts only on the named repositories.
    async fn fresh_with_repos(repos: Option<Vec<String>>) -> (Router, Store) {
        let store = Store::in_memory().await.unwrap();
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let state = AppState {
            store: store.clone(),
            webhook_secret: Arc::new(b"sec".to_vec()),
            metrics: recorder.handle(),
            debounce_secs: 30,
            repos: Arc::new(RepoFilter::new(repos.as_ref())),
            relay: None,
        };
        (router(state), store)
    }

    /// A signed `pull_request` delivery for `owner/repo`.
    fn pr_delivery(owner: &str, repo: &str) -> (String, String) {
        let body = serde_json::json!({
            "action": "opened", "number": 1,
            "installation": { "id": 9 },
            "repository": { "name": repo, "owner": { "login": owner }, "default_branch": "main" },
            "pull_request": {
                "number": 1, "title": "feat: x", "body": "ok",
                "user": { "login": "a" }, "draft": false, "state": "open",
                "head": { "sha": "s1", "ref": "x" }, "base": { "sha": "s0", "ref": "main" }
            }
        })
        .to_string();
        let sig = sign(b"sec", body.as_bytes());
        (body, sig)
    }

    async fn post(app: Router, body: String, sig: String) -> StatusCode {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("X-Hub-Signature-256", sig)
                .header("X-GitHub-Event", "pull_request")
                .header("X-GitHub-Delivery", "d1")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
    }

    #[tokio::test]
    async fn a_repository_that_is_not_listed_never_becomes_a_job() {
        // The Apps are installed on every repository in the account; acting on
        // all of them is GPU-minutes per pull request on a measurement host.
        let (app, store) = fresh_with_repos(Some(vec![
            "brayniac/slipway".into(),
            "brayniac/levinson".into(),
        ]))
        .await;
        let (body, sig) = pr_delivery("brayniac", "some-other-repo");

        // 200, not an error: barry received it and chose not to act.
        assert_eq!(post(app, body, sig).await, StatusCode::OK);
        assert_eq!(store.count_rows("jobs").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_listed_repository_still_gets_through() {
        let (app, store) = fresh_with_repos(Some(vec!["brayniac/slipway".into()])).await;
        let (body, sig) = pr_delivery("brayniac", "slipway");
        assert_eq!(post(app, body, sig).await, StatusCode::OK);
        assert_eq!(store.count_rows("jobs").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn the_allowlist_ignores_case_as_github_does() {
        // GitHub will happily deliver `Brayniac/Slipway` for the repository
        // written `brayniac/slipway`, and a HashSet<String> will not.
        let (app, store) = fresh_with_repos(Some(vec!["brayniac/slipway".into()])).await;
        let (body, sig) = pr_delivery("Brayniac", "Slipway");
        assert_eq!(post(app, body, sig).await, StatusCode::OK);
        assert_eq!(store.count_rows("jobs").await.unwrap(), 1);
    }

    #[test]
    fn no_allowlist_allows_everything() {
        // What barry did before this existed, and what an unset key must keep
        // meaning.
        let f = RepoFilter::new(None);
        assert!(f.allows("anyone", "anything"));
        assert_eq!(f.configured_count(), None);
    }

    async fn fresh() -> (Router, Store) {
        let store = Store::in_memory().await.unwrap();
        let _ = crate::telemetry::init_tracing;
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let metrics = recorder.handle();
        let state = AppState {
            store: store.clone(),
            webhook_secret: Arc::new(b"sec".to_vec()),
            metrics,
            debounce_secs: 30,
            repos: Arc::new(RepoFilter::default()),
            relay: None,
        };
        (router(state), store)
    }

    #[tokio::test]
    async fn rejects_bad_signature() {
        let (app, _store) = fresh().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("X-Hub-Signature-256", "sha256=00")
                    .header("X-GitHub-Event", "ping")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn enqueues_pull_request_opened() {
        let (app, store) = fresh().await;
        let body = serde_json::json!({
            "action": "opened", "number": 1,
            "installation": { "id": 9 },
            "repository": { "name": "r", "owner": { "login": "o" }, "default_branch": "main" },
            "pull_request": {
                "number": 1, "title": "feat: x", "body": "ok",
                "user": { "login": "a" }, "draft": false, "state": "open",
                "head": { "sha": "s1", "ref": "x" }, "base": { "sha": "s0", "ref": "main" }
            }
        })
        .to_string();
        let sig = sign(b"sec", body.as_bytes());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("X-Hub-Signature-256", sig)
                    .header("X-GitHub-Event", "pull_request")
                    .header("X-GitHub-Delivery", "d1")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let n = store.count_rows("jobs").await.unwrap();
        assert_eq!(n, 1);
    }
}
