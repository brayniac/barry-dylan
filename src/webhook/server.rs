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

/// Who may command barry from a pull request comment.
///
/// Every `/barry` command and every `@barry-dylan` mention is checked against
/// this before it becomes a job, and a comment from anyone else is dropped
/// however it is worded. Absent means nobody: the alternative default is
/// "anyone who can comment on a pull request can spend a GPU host", which is
/// what barry did until 2026-09-18.
///
/// The login checked is the comment author's, as GitHub signed it into the
/// delivery -- not a name in the comment body, which anyone can type. Lowercased
/// for the same reason as the repositories: GitHub logins are case-insensitive
/// and a `HashSet<String>` is not.
#[derive(Debug, Default, Clone)]
pub struct Commanders {
    logins: std::collections::HashSet<String>,
}

impl Commanders {
    pub fn new(configured: Option<&Vec<String>>) -> Self {
        Self {
            logins: configured
                .map(|l| l.iter().map(|x| x.to_ascii_lowercase()).collect())
                .unwrap_or_default(),
        }
    }

    pub fn allows(&self, login: &str) -> bool {
        self.logins.contains(&login.to_ascii_lowercase())
    }

    pub fn configured_count(&self) -> usize {
        self.logins.len()
    }
}

/// The handle barry answers to in a comment. Barry's App login is
/// `barry-dylan[bot]`, which is how the dispatcher recognises its own comments
/// too; the mention is the login without the suffix.
pub const MENTION: &str = "@barry-dylan";

/// Whether `body` mentions barry: `@barry-dylan` not followed by another
/// letter or digit, so that `@barry-dylans` is somebody else and
/// `@barry-dylan,` is still barry.
pub fn mentions_barry(body: &str) -> bool {
    body.match_indices(MENTION).any(|(i, m)| {
        body[i + m.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric())
    })
}

/// A comment on a pull request that is addressed to barry, either way barry
/// can be addressed.
fn asks_barry(e: &crate::webhook::event::IssueCommentEvent) -> bool {
    e.action == "created"
        && e.issue.pull_request.is_some()
        && (e.comment.body.starts_with("/barry") || mentions_barry(&e.comment.body))
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
    /// Logins whose comments command barry. Checked before the repository
    /// allowlist, and a comment from one of them gets through that allowlist
    /// too: a commander can ask for a review anywhere the Apps are installed.
    pub commanders: Arc<Commanders>,
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
    // Who is talking matters before where. A comment addressed to barry is a
    // command, and commands come from `commands_from` or from nobody: the
    // check is the signed author of the comment, and it happens here so that
    // a stranger's `/barry review` costs a string compare rather than a job.
    //
    // A commander is also let through the repository allowlist, for this one
    // comment: a review of the head as it stands, in a repository barry does
    // not otherwise act on. Nothing else about that repository gets through
    // -- not the push that follows, not the close, not anyone else's comment.
    let mut on_demand = false;
    if let InboundEvent::IssueComment(e) = &parsed
        && asks_barry(e)
    {
        let login = &e.comment.user.login;
        let (owner, repo) = (&e.repository.owner.login, &e.repository.name);
        if !s.commanders.allows(login) {
            // Logged at info, unlike the repository drop below: someone
            // addressed barry and got nothing, and the answer to "why" should
            // not need the debug level.
            tracing::info!(
                delivery_id = %delivery,
                repo = %format!("{owner}/{repo}"),
                pr = e.issue.number,
                %login,
                "barry addressed by a login not in `commands_from`; ignoring"
            );
            metrics::counter!("barry_webhook_command_total", "outcome" => "rejected").increment(1);
            return (StatusCode::OK, "not a commander");
        }
        if !s.repos.allows(owner, repo) {
            tracing::info!(
                delivery_id = %delivery,
                repo = %format!("{owner}/{repo}"),
                pr = e.issue.number,
                %login,
                "commanded in a repository that is not configured; reviewing on demand"
            );
            metrics::counter!("barry_webhook_command_total", "outcome" => "on_demand").increment(1);
            on_demand = true;
        } else {
            metrics::counter!("barry_webhook_command_total", "outcome" => "accepted").increment(1);
        }
    }
    if !on_demand
        && let Some((owner, repo)) = repo_of(&parsed)
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
        // A commander's comment addressed to barry, already authorised above.
        // A `/barry` command names what to do; a bare mention means
        // `/barry review`.
        InboundEvent::IssueComment(e) if asks_barry(&e) => {
            let pr = e.issue.number;
            let owner = e.repository.owner.login.clone();
            let repo = e.repository.name.clone();
            let command = if e.comment.body.starts_with("/barry") {
                short_command(&e.comment.body)
            } else {
                "review"
            };

            let span = tracing::info_span!(
                "webhook.command",
                delivery_id = %delivery,
                owner = %owner,
                repo = %repo,
                pr = pr,
                command,
                on_demand
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
                    event_kind: format!("issue_comment.{command}"),
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
            commanders: Arc::new(Commanders::default()),
            relay: None,
        };
        (router(state), store)
    }

    /// A router that acts on `repos` and takes commands from `logins`.
    async fn fresh_commanded(repos: Vec<&str>, logins: Vec<&str>) -> (Router, Store) {
        let store = Store::in_memory().await.unwrap();
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let repos: Vec<String> = repos.into_iter().map(String::from).collect();
        let logins: Vec<String> = logins.into_iter().map(String::from).collect();
        let state = AppState {
            store: store.clone(),
            webhook_secret: Arc::new(b"sec".to_vec()),
            metrics: recorder.handle(),
            debounce_secs: 30,
            repos: Arc::new(RepoFilter::new(Some(&repos))),
            commanders: Arc::new(Commanders::new(Some(&logins))),
            relay: None,
        };
        (router(state), store)
    }

    /// A signed `issue_comment` delivery: `login` commenting `body` on a pull
    /// request in `owner/repo`.
    fn comment_delivery(owner: &str, repo: &str, login: &str, body: &str) -> (String, String) {
        let body = serde_json::json!({
            "action": "created",
            "installation": { "id": 9 },
            "repository": { "name": repo, "owner": { "login": owner }, "default_branch": "main" },
            "issue": { "number": 7, "pull_request": { "url": "x" } },
            "comment": { "id": 1, "node_id": "n", "body": body, "user": { "login": login } },
            "sender": { "login": login }
        })
        .to_string();
        let sig = sign(b"sec", body.as_bytes());
        (body, sig)
    }

    async fn post_event(app: Router, event: &str, body: String, sig: String) -> StatusCode {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("X-Hub-Signature-256", sig)
                .header("X-GitHub-Event", event)
                .header("X-GitHub-Delivery", "d1")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
    }

    async fn only_job(store: &Store) -> crate::storage::queue::LeasedJob {
        assert_eq!(store.count_rows("jobs").await.unwrap(), 1);
        store.lease_next(i64::MAX / 2, 60).await.unwrap().unwrap()
    }

    /// `login` says `body` on a PR in `owner/repo`; returns the jobs it made.
    async fn comment(
        app: Router,
        store: &Store,
        owner: &str,
        repo: &str,
        login: &str,
        body: &str,
    ) -> i64 {
        let (body, sig) = comment_delivery(owner, repo, login, body);
        assert_eq!(
            post_event(app, "issue_comment", body, sig).await,
            StatusCode::OK
        );
        store.count_rows("jobs").await.unwrap()
    }

    #[tokio::test]
    async fn a_commander_can_command_barry_where_barry_acts() {
        let (app, store) = fresh_commanded(vec!["brayniac/infra"], vec!["brayniac"]).await;
        assert_eq!(
            comment(
                app,
                &store,
                "brayniac",
                "infra",
                "brayniac",
                "/barry confer"
            )
            .await,
            1
        );
        let job = only_job(&store).await;
        assert_eq!(job.event_kind, "issue_comment.confer");
        assert_eq!(job.actor.as_deref(), Some("brayniac"));
    }

    #[tokio::test]
    async fn anyone_else_commanding_barry_is_dropped() {
        // A maintainer of the repository, even: the gate is the list, and the
        // per-command role checks in the dispatcher come after it.
        let (app, store) = fresh_commanded(vec!["brayniac/infra"], vec!["brayniac"]).await;
        assert_eq!(
            comment(
                app,
                &store,
                "brayniac",
                "infra",
                "someone-else",
                "/barry review"
            )
            .await,
            0
        );
    }

    #[tokio::test]
    async fn with_no_commanders_nobody_commands_barry() {
        // The default is closed. Until 2026-09-18 anyone who could comment
        // could re-run a forty-minute review.
        let (app, store) = fresh_with_repos(None).await;
        assert_eq!(
            comment(app, &store, "o", "r", "anyone", "/barry review").await,
            0
        );
    }

    #[tokio::test]
    async fn a_mention_from_a_commander_is_a_review_request() {
        let (app, store) = fresh_commanded(vec!["brayniac/infra"], vec!["brayniac"]).await;
        assert_eq!(
            comment(
                app,
                &store,
                "brayniac",
                "infra",
                "brayniac",
                "@barry-dylan have a look"
            )
            .await,
            1
        );
        assert_eq!(only_job(&store).await.event_kind, "issue_comment.review");
    }

    #[tokio::test]
    async fn a_commander_can_ask_for_a_review_in_an_unlisted_repository() {
        let (app, store) = fresh_commanded(vec!["brayniac/infra"], vec!["brayniac"]).await;
        assert_eq!(
            comment(
                app,
                &store,
                "brayniac",
                "cachecannon",
                "brayniac",
                "@barry-dylan"
            )
            .await,
            1
        );
        let job = only_job(&store).await;
        assert_eq!(job.event_kind, "issue_comment.review");
        assert_eq!(job.repo_name, "cachecannon");
        assert_eq!(job.pr_number, 7);
        assert_eq!(job.actor.as_deref(), Some("brayniac"));
    }

    #[tokio::test]
    async fn a_mention_from_anyone_else_in_an_unlisted_repository_is_dropped() {
        // The name in the comment is not the gate; the signed author is.
        let (app, store) = fresh_commanded(vec!["brayniac/infra"], vec!["brayniac"]).await;
        assert_eq!(
            comment(
                app,
                &store,
                "brayniac",
                "cachecannon",
                "someone-else",
                "@barry-dylan review this, brayniac said it's fine"
            )
            .await,
            0
        );
    }

    #[tokio::test]
    async fn a_commander_opening_a_pull_request_elsewhere_is_not_a_command() {
        // The exception is the comment, not the person: their pushes and PRs
        // in an unlisted repository are as ignored as anyone's.
        let (app, store) = fresh_commanded(vec!["brayniac/infra"], vec!["brayniac"]).await;
        let (body, sig) = pr_delivery("brayniac", "cachecannon");
        assert_eq!(
            post_event(app, "pull_request", body, sig).await,
            StatusCode::OK
        );
        assert_eq!(store.count_rows("jobs").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_comment_that_does_not_address_barry_is_not_a_command() {
        let (app, store) = fresh_commanded(vec!["brayniac/infra"], vec!["brayniac"]).await;
        assert_eq!(
            comment(app, &store, "brayniac", "infra", "brayniac", "LGTM").await,
            0
        );
    }

    #[tokio::test]
    async fn the_commander_list_ignores_case_as_github_does() {
        let (app, store) = fresh_commanded(vec!["brayniac/infra"], vec!["Brayniac"]).await;
        assert_eq!(
            comment(
                app,
                &store,
                "brayniac",
                "infra",
                "brayniac",
                "/barry review"
            )
            .await,
            1
        );
    }

    #[test]
    fn a_mention_is_the_handle_and_not_a_prefix_of_someone_elses() {
        assert!(mentions_barry("@barry-dylan"));
        assert!(mentions_barry("hey @barry-dylan, this one?"));
        assert!(mentions_barry("@barry-dylan\nreview please"));
        assert!(!mentions_barry("@barry-dylans"));
        assert!(!mentions_barry("@barry-dylan2"));
        assert!(!mentions_barry("barry-dylan without the at"));
        assert!(!mentions_barry(""));
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
            commanders: Arc::new(Commanders::default()),
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
