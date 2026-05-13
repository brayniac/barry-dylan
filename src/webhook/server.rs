use crate::storage::queue::NewJob;
use crate::storage::Store;
use crate::webhook::event::{parse, InboundEvent};
use crate::webhook::verify;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use metrics_exporter_prometheus::PrometheusHandle;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub webhook_secret: Arc<Vec<u8>>,
    pub metrics: PrometheusHandle,
    pub debounce_secs: u64,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/webhook", post(webhook))
        .with_state(state)
}

async fn healthz(State(s): State<AppState>) -> impl IntoResponse {
    match sqlx::query("SELECT 1").execute(&s.store.pool).await {
        Ok(_) => (StatusCode::OK, "ok"),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "db unavailable"),
    }
}

async fn metrics(State(s): State<AppState>) -> impl IntoResponse {
    (StatusCode::OK, s.metrics.render())
}

async fn webhook(State(s): State<AppState>, headers: HeaderMap, body: Bytes)
    -> impl IntoResponse
{
    metrics::counter!("barry_webhook_received_total").increment(1);

    let sig = headers.get("X-Hub-Signature-256").and_then(|v| v.to_str().ok());
    if let Err(e) = verify::verify(&s.webhook_secret, &body, sig) {
        tracing::warn!(?e, "webhook signature verification failed");
        metrics::counter!("barry_webhook_rejected_total", "reason" => "signature").increment(1);
        return (StatusCode::UNAUTHORIZED, "bad signature");
    }
    let evt = headers.get("X-GitHub-Event").and_then(|v| v.to_str().ok());
    let delivery = headers.get("X-GitHub-Delivery").and_then(|v| v.to_str().ok())
        .unwrap_or("unknown").to_string();
    let parsed = match parse(evt, &body) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(?e, event = evt, delivery_id = %delivery, "webhook parse failed");
            metrics::counter!("barry_webhook_rejected_total", "reason" => "parse").increment(1);
            return (StatusCode::BAD_REQUEST, "bad payload");
        }
    };
    tracing::info!(event = evt, delivery_id = %delivery, "webhook received");

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    let debounce = s.debounce_secs as i64;

    let to_enqueue: Option<(NewJob, i64)> = match parsed {
        InboundEvent::PullRequest(e) if pull_request_action_is_actionable(&e.action) => {
            Some((NewJob {
                installation_id: e.installation.id,
                repo_owner: e.repository.owner.login,
                repo_name: e.repository.name,
                pr_number: e.number,
                event_kind: format!("pull_request.{}", e.action),
                delivery_id: delivery.clone(),
            }, if e.action == "synchronize" { now + debounce } else { now }))
        }
        InboundEvent::IssueComment(e) if e.action == "created" && e.issue.pull_request.is_some()
            && e.comment.body.starts_with("/barry") =>
        {
            Some((NewJob {
                installation_id: e.installation.id,
                repo_owner: e.repository.owner.login,
                repo_name: e.repository.name,
                pr_number: e.issue.number,
                event_kind: format!("issue_comment.{}", short_command(&e.comment.body)),
                delivery_id: delivery.clone(),
            }, now))
        }
        _ => None,
    };

    if let Some((job, run_after)) = to_enqueue {
        let owner = job.repo_owner.clone();
        let repo = job.repo_name.clone();
        let pr = job.pr_number;
        let kind = job.event_kind.clone();
        if let Err(e) = s.store.enqueue(&job, now, run_after).await {
            tracing::error!(?e, %owner, %repo, pr, event_kind = %kind, "enqueue failed");
            metrics::counter!("barry_webhook_rejected_total", "reason" => "enqueue").increment(1);
            return (StatusCode::INTERNAL_SERVER_ERROR, "enqueue failed");
        }
        metrics::counter!("barry_job_enqueued_total").increment(1);
        tracing::info!(
            %owner, %repo, pr, event_kind = %kind,
            delivery_id = %delivery, run_after_in_secs = run_after - now,
            "job enqueued",
        );
    } else {
        tracing::debug!(event = evt, delivery_id = %delivery, "event dropped (not actionable)");
    }
    (StatusCode::OK, "ok")
}

fn pull_request_action_is_actionable(a: &str) -> bool {
    matches!(a, "opened" | "synchronize" | "reopened" | "ready_for_review")
}

fn short_command(body: &str) -> &'static str {
    let first = body.split_whitespace().nth(1).unwrap_or("");
    match first {
        "approve" => "approve",
        "review" => "review",
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
        for b in bytes { s.push_str(&format!("{b:02x}")); }
        s
    }

    async fn fresh() -> (Router, Store) {
        let store = Store::in_memory().await.unwrap();
        let _ = crate::telemetry::init_tracing;
        // Use build_recorder() + handle() instead of install_recorder() to avoid
        // SetRecorderError when multiple tests run in parallel (the recorder is global).
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new()
            .build_recorder();
        let metrics = recorder.handle();
        let state = AppState {
            store: store.clone(),
            webhook_secret: Arc::new(b"sec".to_vec()),
            metrics,
            debounce_secs: 30,
        };
        (router(state), store)
    }

    #[tokio::test]
    async fn rejects_bad_signature() {
        let (app, _store) = fresh().await;
        let resp = app.oneshot(Request::builder()
            .method("POST").uri("/webhook")
            .header("X-Hub-Signature-256", "sha256=00")
            .header("X-GitHub-Event", "ping")
            .body(Body::from("{}")).unwrap()).await.unwrap();
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
        }).to_string();
        let sig = sign(b"sec", body.as_bytes());
        let resp = app.oneshot(Request::builder()
            .method("POST").uri("/webhook")
            .header("X-Hub-Signature-256", sig)
            .header("X-GitHub-Event", "pull_request")
            .header("X-GitHub-Delivery", "d1")
            .body(Body::from(body)).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM jobs")
            .fetch_one(&store.pool).await.unwrap();
        assert_eq!(n, 1);
    }
}
