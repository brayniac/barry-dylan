//! The relay, end to end, against a stand-in for smee.
//!
//! The unit tests in `src/relay.rs` pin the parsing and the re-serialization.
//! What they cannot show is the claim the deployment rests on: that a delivery
//! GitHub signed survives being parsed by smee, re-serialized here, posted to
//! barry's own endpoint, verified there, and enqueued as a job.
//!
//! Every step of that is real except smee itself, which is an axum route
//! emitting one frame in smee's own shape.

use axum::response::sse::{Event, Sse};
use axum::routing::get;
use barry_dylan::relay::{self, RelayConfig};
use barry_dylan::storage::Store;
use barry_dylan::webhook::server::{AppState, RepoFilter, router};
use barry_dylan::webhook::verify;
use futures::stream;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const SECRET: &[u8] = b"a-shared-secret";

/// A `pull_request` payload with keys in an order a sorting serializer would
/// change, so that an ordering regression fails this test rather than
/// production.
const BODY: &str = r#"{"zaction":"opened","number":7,"installation":{"id":1234},"repository":{"name":"r","owner":{"login":"o"},"default_branch":"main"},"action":"opened","pull_request":{"number":7,"title":"feat: a thing","body":"a description long enough to pass hygiene","user":{"login":"someone"},"draft":false,"state":"open","head":{"sha":"abc123","ref":"topic"},"base":{"sha":"def456","ref":"main"}}}"#;

/// One SSE frame carrying one delivery, shaped the way smee shapes it: the
/// headers hoisted to top level, the body nested as parsed JSON.
fn smee_envelope() -> String {
    let signature = verify::sign(SECRET, BODY.as_bytes());
    format!(
        r#"{{"host":"smee.io","content-type":"application/json","x-github-event":"pull_request","x-github-delivery":"delivery-1","x-hub-signature-256":"{signature}","body":{BODY},"timestamp":1757894400000}}"#
    )
}

/// Serve exactly one delivery, then hold the connection open.
async fn fake_smee() -> String {
    let app = axum::Router::new().route(
        "/channel",
        get(|| async {
            let events = stream::iter(vec![
                Ok::<_, std::convert::Infallible>(Event::default().event("ready").data("{}")),
                Ok(Event::default().data(smee_envelope())),
            ]);
            // keep_alive stops the stream ending after the last item, which is
            // what a real channel does: it stays open and quiet.
            Sse::new(events).keep_alive(axum::response::sse::KeepAlive::default())
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}/channel")
}

#[tokio::test]
async fn a_delivery_survives_the_channel_and_becomes_a_job() {
    let store = Store::in_memory().await.unwrap();
    let metrics = metrics_exporter_prometheus::PrometheusBuilder::new()
        .build_recorder()
        .handle();
    let state = AppState {
        store: store.clone(),
        webhook_secret: Arc::new(SECRET.to_vec()),
        metrics,
        debounce_secs: 0,
        // No allowlist: this test is about the relay, not about which
        // repositories barry acts on.
        repos: Arc::new(RepoFilter::default()),
        relay: None,
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let barry_addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await });

    let channel = fake_smee().await;
    let status = Arc::new(relay::Status::default());
    let shutdown = CancellationToken::new();
    tokio::spawn(relay::run(
        RelayConfig {
            smee_url: channel,
            require_signature: true,
        },
        format!("http://{barry_addr}/webhook"),
        Arc::new(SECRET.to_vec()),
        status.clone(),
        shutdown.clone(),
    ));

    // The relay connects, reads, and posts; none of it is instant and none of
    // it is slow. Poll rather than sleep so a fast machine does not wait.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let job = loop {
        let now = barry_dylan::util::now_ts();
        if let Some(job) = store.lease_next(now, 60).await.unwrap() {
            break job;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no job arrived: relay connected = {}, events = {}",
            status.connected(),
            status.events()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    shutdown.cancel();

    assert_eq!(job.repo_owner, "o");
    assert_eq!(job.repo_name, "r");
    assert_eq!(job.pr_number, 7);
    assert_eq!(job.event_kind, "pull_request.opened");
    // Barry's own delivery id, not one the relay invented: it is what ties a
    // log line here to a delivery in GitHub's log.
    assert_eq!(job.delivery_id, "delivery-1");
    assert_eq!(status.events(), 1);
}
