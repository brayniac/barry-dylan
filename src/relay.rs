//! Receiving webhooks on a rack GitHub cannot reach.
//!
//! GitHub posts to a smee.io channel; barry holds that channel open from the
//! inside and feeds what arrives to its own `/webhook`. Nothing inbound is
//! needed: delta answers on the LAN only, and every way to change that costs a
//! network project — a tailnet to log into, a domain to buy, or a port
//! forwarded to the internet. rack-ci, on this same host, polls GitHub for
//! exactly this reason; barry cannot, because its slash commands arrive as
//! `issue_comment` and polling those means listing comments on every open pull
//! request forever.
//!
//! Events are re-POSTed to barry's own endpoint rather than handed to
//! [`crate::webhook`] in process. A smee channel is a public URL — anyone who
//! learns it can post to it — so the HMAC check is the only thing between that
//! channel and the job queue, and skipping it here would skip it in the one
//! component that most needs it.

use crate::webhook::verify;
use serde::Deserialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// How barry is told about events when it cannot be reached directly.
#[derive(Debug, Clone, Deserialize)]
pub struct RelayConfig {
    /// The smee.io channel GitHub posts to, e.g. `https://smee.io/aBcDeF`.
    pub smee_url: String,
    /// Forward GitHub's own signature and let the webhook handler reject what
    /// does not verify. Default, and what you want.
    ///
    /// smee does not forward raw bytes — it parses the JSON body and re-emits
    /// it — so the signature only survives if the re-serialization is
    /// byte-identical to what GitHub signed. That holds in the ordinary case
    /// (see [`body_bytes`]) and `barry_relay_rejected_total` counts when it
    /// does not.
    ///
    /// Setting this false makes the relay re-sign what it forwards, which moves
    /// the trust boundary to the channel URL: anything posted to the channel is
    /// then accepted as if GitHub had sent it. It exists for the case where a
    /// payload systematically fails to round-trip, and it should be a temporary
    /// answer.
    #[serde(default = "default_require_signature")]
    pub require_signature: bool,
}

fn default_require_signature() -> bool {
    true
}

/// What one delivery from the channel carries.
///
/// smee wraps the original request: the headers become top-level fields and the
/// body becomes a nested JSON object. Only the fields the webhook handler needs
/// are modelled; a delivery carries a dozen more (host, query, timestamp) that
/// barry has no use for.
#[derive(Debug, Deserialize)]
pub struct SmeeEvent {
    #[serde(rename = "x-github-event")]
    pub event: String,
    #[serde(rename = "x-github-delivery")]
    #[serde(default)]
    pub delivery: Option<String>,
    #[serde(rename = "x-hub-signature-256")]
    #[serde(default)]
    pub signature: Option<String>,
    pub body: serde_json::Value,
}

/// Whether the relay is connected, and when it last saw anything.
///
/// Reported by `/healthz` because smee keeps **no backlog**: an event delivered
/// while the relay is disconnected is gone, and GitHub still records a `200`
/// from smee itself. Nothing goes red anywhere, so liveness has to be asked
/// for rather than waited for.
#[derive(Debug, Default)]
pub struct Status {
    connected: AtomicBool,
    last_event_unix: AtomicI64,
    events: AtomicU64,
}

impl Status {
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn events(&self) -> u64 {
        self.events.load(Ordering::Relaxed)
    }

    /// Unix seconds of the last event, or `None` if there has not been one.
    pub fn last_event_unix(&self) -> Option<i64> {
        match self.last_event_unix.load(Ordering::Relaxed) {
            0 => None,
            t => Some(t),
        }
    }

    fn set_connected(&self, yes: bool) {
        self.connected.store(yes, Ordering::Relaxed);
        metrics::gauge!("barry_relay_connected").set(if yes { 1.0 } else { 0.0 });
    }

    fn saw_event(&self) {
        self.events.fetch_add(1, Ordering::Relaxed);
        self.last_event_unix
            .store(crate::util::now_ts(), Ordering::Relaxed);
    }
}

/// Pull one event out of an SSE frame, or decide it is not one.
///
/// smee sends `ready` when the channel opens and `ping` to keep the connection
/// alive; both are frames with no delivery in them. An unparseable frame is
/// also `None` rather than an error: the stream is long-lived, and one bad
/// frame is not a reason to tear down a connection that is otherwise working.
pub fn parse_frame(frame: &str) -> Option<SmeeEvent> {
    let mut kind = "message";
    let mut data = String::new();
    for line in frame.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            kind = rest.trim();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
    }
    if kind != "message" || data.is_empty() {
        return None;
    }
    match serde_json::from_str(&data) {
        Ok(ev) => Some(ev),
        Err(e) => {
            tracing::debug!(error = %e, "ignoring a frame that is not a delivery");
            None
        }
    }
}

/// The bytes to forward as the body.
///
/// Compact, and in the order the delivery arrived in. `serde_json::Value` is
/// backed by an order-preserving map here (the `preserve_order` feature is on
/// for exactly this), because its default `BTreeMap` sorts keys — which would
/// re-order every payload and fail every signature, since GitHub signed its own
/// ordering.
pub fn body_bytes(ev: &SmeeEvent) -> Vec<u8> {
    serde_json::to_vec(&ev.body).unwrap_or_default()
}

/// Where the relay posts what it receives.
///
/// A listen address is what to bind, which is not always somewhere you can send
/// to: `0.0.0.0` and `::` mean "every interface" to a listener and nothing at
/// all to a client. Loopback is both correct and the point — the relay is
/// talking to its own process.
pub fn target_url(listen: &str) -> String {
    let (host, port) = match listen.rsplit_once(':') {
        Some((h, p)) => (h.trim_matches(['[', ']']), p),
        None => ("127.0.0.1", listen),
    };
    let host = match host {
        "" | "0.0.0.0" | "::" | "*" => "127.0.0.1",
        h => h,
    };
    format!("http://{host}:{port}/webhook")
}

/// How long to wait before reconnecting after `attempt` consecutive failures.
///
/// Capped at a minute: a relay that backs off further would sit out an outage
/// it could have ridden through, and unlike a build there is nothing waiting on
/// the other side of the delay.
pub fn backoff(attempt: u32) -> Duration {
    const CAP: u64 = 60;
    Duration::from_secs((1u64 << attempt.min(6)).min(CAP))
}

/// Hold the channel open, forever, feeding deliveries to `target`.
pub async fn run(
    cfg: RelayConfig,
    target: String,
    secret: Arc<Vec<u8>>,
    status: Arc<Status>,
    shutdown: CancellationToken,
) {
    if !cfg.require_signature {
        tracing::warn!(
            "relay.require_signature is false: the relay re-signs what it forwards, so \
             anything posted to the channel URL is accepted as if GitHub had sent it"
        );
    }
    tracing::info!(channel = %cfg.smee_url, target = %target, "relaying webhooks");

    // No request timeout: this connection is meant to stay open. A read timeout
    // instead, so a connection that has gone quiet without closing (which is
    // what a dropped NAT entry looks like from in here) is noticed and retried
    // rather than held forever. smee pings well inside this.
    let http = match reqwest::Client::builder()
        .read_timeout(Duration::from_secs(120))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "could not build the relay http client; relay disabled");
            return;
        }
    };

    let mut attempt = 0u32;
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        match connect_and_stream(&http, &cfg, &target, &secret, &status, &shutdown).await {
            Ok(()) => {
                // A clean end of stream is normal: smee drops long-lived
                // connections periodically. Reconnect without penalty.
                attempt = 0;
            }
            Err(e) => {
                tracing::warn!(error = %e, attempt, "relay connection failed");
                metrics::counter!("barry_relay_disconnects_total").increment(1);
                attempt = attempt.saturating_add(1);
            }
        }
        status.set_connected(false);
        if shutdown.is_cancelled() {
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff(attempt)) => {}
            _ = shutdown.cancelled() => return,
        }
    }
}

async fn connect_and_stream(
    http: &reqwest::Client,
    cfg: &RelayConfig,
    target: &str,
    secret: &[u8],
    status: &Status,
    shutdown: &CancellationToken,
) -> anyhow::Result<()> {
    use futures::StreamExt;

    let resp = http
        .get(&cfg.smee_url)
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .send()
        .await?
        .error_for_status()?;

    status.set_connected(true);
    metrics::counter!("barry_relay_connects_total").increment(1);
    tracing::info!(channel = %cfg.smee_url, "relay connected");

    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    loop {
        let chunk = tokio::select! {
            c = stream.next() => c,
            _ = shutdown.cancelled() => return Ok(()),
        };
        let Some(chunk) = chunk else {
            return Ok(());
        };
        buf.push_str(&String::from_utf8_lossy(&chunk?));

        // Frames are separated by a blank line. Anything after the last one is
        // a partial frame and stays in the buffer.
        while let Some(end) = buf.find("\n\n") {
            let frame = buf[..end].to_string();
            buf.drain(..end + 2);
            if let Some(ev) = parse_frame(&frame) {
                status.saw_event();
                metrics::counter!("barry_relay_events_total", "event" => ev.event.clone())
                    .increment(1);
                deliver(http, target, &ev, cfg, secret).await;
            }
        }
    }
}

/// Forward one delivery, and say what happened to it.
///
/// Failure is logged and dropped rather than retried: the webhook handler
/// enqueues and returns, so a failure here is barry's own endpoint being
/// unreachable or refusing the payload, and neither improves by being asked
/// again immediately.
async fn deliver(
    http: &reqwest::Client,
    target: &str,
    ev: &SmeeEvent,
    cfg: &RelayConfig,
    secret: &[u8],
) {
    let body = body_bytes(ev);
    let delivery = ev.delivery.clone().unwrap_or_else(|| "relayed".to_string());

    let signature = if cfg.require_signature {
        ev.signature.clone()
    } else {
        Some(verify::sign(secret, &body))
    };

    let mut req = http
        .post(target)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("X-GitHub-Event", &ev.event)
        .header("X-GitHub-Delivery", &delivery);
    if let Some(sig) = &signature {
        req = req.header("X-Hub-Signature-256", sig);
    }

    match req.body(body).send().await {
        Ok(resp) if resp.status().is_success() => {
            tracing::debug!(event = %ev.event, delivery_id = %delivery, "relayed");
        }
        Ok(resp) => {
            let status = resp.status();
            // 401 is the interesting one: the body did not survive smee's
            // round trip byte-identically, so GitHub's signature no longer
            // matches what arrived. Counted separately because a systematic
            // break here is silent otherwise -- GitHub keeps seeing 200 from
            // smee, and barry keeps receiving nothing.
            let reason = if status == reqwest::StatusCode::UNAUTHORIZED {
                "signature"
            } else {
                "status"
            };
            metrics::counter!("barry_relay_rejected_total", "reason" => reason).increment(1);
            tracing::warn!(
                event = %ev.event,
                delivery_id = %delivery,
                %status,
                "barry refused a relayed delivery"
            );
        }
        Err(e) => {
            metrics::counter!("barry_relay_rejected_total", "reason" => "unreachable").increment(1);
            tracing::warn!(error = %e, delivery_id = %delivery, "could not relay to barry");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DELIVERY: &str = r#"{"x-github-event":"pull_request","x-github-delivery":"abc-123","x-hub-signature-256":"sha256=deadbeef","body":{"action":"opened","number":7}}"#;

    #[test]
    fn a_message_frame_yields_its_delivery() {
        let ev = parse_frame(&format!("data: {DELIVERY}")).expect("a delivery");
        assert_eq!(ev.event, "pull_request");
        assert_eq!(ev.delivery.as_deref(), Some("abc-123"));
        assert_eq!(ev.body["number"], 7);
    }

    #[test]
    fn a_ping_frame_is_not_a_delivery() {
        assert!(parse_frame("event: ping\ndata: 1757894400000").is_none());
    }

    #[test]
    fn a_ready_frame_is_not_a_delivery() {
        assert!(parse_frame("event: ready\ndata: {}").is_none());
    }

    #[test]
    fn a_frame_that_is_not_json_is_ignored_rather_than_fatal() {
        // One bad frame must not take down a connection that is otherwise fine.
        assert!(parse_frame("data: <html>nope</html>").is_none());
    }

    #[test]
    fn a_frame_missing_the_event_header_is_not_a_delivery() {
        assert!(parse_frame(r#"data: {"body":{"action":"opened"}}"#).is_none());
    }

    #[test]
    fn multi_line_data_is_joined_with_newlines() {
        let frame = "data: {\"x-github-event\":\"push\",\ndata: \"body\":{}}";
        let ev = parse_frame(frame).expect("a delivery");
        assert_eq!(ev.event, "push");
    }

    #[test]
    fn a_relayed_body_keeps_githubs_key_order() {
        // The whole signature story rests on this: serde_json's default map
        // sorts keys, and a sorted payload is a payload GitHub did not sign.
        let ev = parse_frame(&format!("data: {DELIVERY}")).unwrap();
        assert_eq!(
            String::from_utf8(body_bytes(&ev)).unwrap(),
            r#"{"action":"opened","number":7}"#
        );
    }

    #[test]
    fn a_delivery_signed_by_github_still_verifies_after_the_round_trip() {
        // The claim the whole design rests on. smee parses the body and
        // re-emits it, so the bytes GitHub signed are gone; what arrives at
        // barry is this crate's re-serialization of a parsed value. If that
        // ever stops matching byte-for-byte, every delivery is rejected as a
        // bad signature and the failure looks like silence.
        let secret = b"s3cret";
        let original = r#"{"action":"opened","number":7,"pull_request":{"id":42,"draft":false}}"#;
        let sig = verify::sign(secret, original.as_bytes());
        let envelope = format!(
            r#"{{"x-github-event":"pull_request","x-github-delivery":"d-1","x-hub-signature-256":"{sig}","body":{original}}}"#
        );

        let ev = parse_frame(&format!("data: {envelope}")).expect("a delivery");
        verify::verify(secret, &body_bytes(&ev), ev.signature.as_deref())
            .expect("GitHub's signature must survive smee's round trip");
    }

    #[test]
    fn a_relayed_body_is_compact() {
        let ev = parse_frame(&format!("data: {DELIVERY}")).unwrap();
        let bytes = body_bytes(&ev);
        assert!(!bytes.contains(&b' '), "GitHub does not sign pretty JSON");
    }

    #[test]
    fn the_target_is_loopback_when_listening_on_every_interface() {
        assert_eq!(target_url("0.0.0.0:8181"), "http://127.0.0.1:8181/webhook");
        assert_eq!(target_url("[::]:8181"), "http://127.0.0.1:8181/webhook");
    }

    #[test]
    fn an_explicit_listen_host_is_kept() {
        assert_eq!(
            target_url("127.0.0.1:9000"),
            "http://127.0.0.1:9000/webhook"
        );
    }

    #[test]
    fn backoff_grows_and_then_stops_growing() {
        assert_eq!(backoff(0), Duration::from_secs(1));
        assert_eq!(backoff(3), Duration::from_secs(8));
        // A relay that backed off further would sit out an outage it could
        // have ridden through.
        assert_eq!(backoff(20), Duration::from_secs(60));
    }

    #[test]
    fn require_signature_defaults_to_on() {
        let cfg: RelayConfig = toml::from_str(r#"smee_url = "https://smee.io/x""#).unwrap();
        assert!(cfg.require_signature);
    }
}
