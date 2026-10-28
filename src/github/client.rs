use reqwest::header::HeaderMap;
use reqwest::{Method, Response};
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Cache entry for a permission lookup.
struct PermEntry {
    permission: String,
    expires_at: Instant,
}

type PermKey = (String, String, String);
type PermMap = HashMap<PermKey, PermEntry>;

/// In-memory TTL cache for `(owner, repo, user) -> permission` lookups.
/// Keys include repo since GitHub permissions are repository-scoped.
#[derive(Clone, Default)]
pub struct PermissionCache {
    inner: Arc<Mutex<PermMap>>,
}

impl PermissionCache {
    pub(crate) fn get(&self, owner: &str, repo: &str, user: &str) -> Option<String> {
        let mut map = self.inner.lock().unwrap();
        let key: PermKey = (owner.to_string(), repo.to_string(), user.to_string());
        let entry = map.get(&key)?;
        if entry.expires_at <= Instant::now() {
            map.remove(&key);
            return None;
        }
        Some(entry.permission.clone())
    }

    pub(crate) fn put(&self, owner: &str, repo: &str, user: &str, permission: String) {
        let mut map = self.inner.lock().unwrap();
        map.insert(
            (owner.to_string(), repo.to_string(), user.to_string()),
            PermEntry {
                permission,
                expires_at: Instant::now() + Duration::from_secs(300),
            },
        );
    }
}

/// How long a write to GitHub keeps retrying a transient failure.
#[derive(Debug, Clone, Copy)]
pub struct WriteRetry {
    /// No wait starts that would end later than this after the first attempt.
    pub deadline: Duration,
    /// The first backoff wait. Each later one doubles, up to `max_delay`.
    pub base_delay: Duration,
    pub max_delay: Duration,
    /// The least wait before each lookup of whether a write that may have
    /// been applied landed, so that GitHub has time to show it.
    pub lookup_settle: Duration,
}

impl Default for WriteRetry {
    fn default() -> Self {
        Self {
            deadline: Duration::from_secs(300),
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            lookup_settle: Duration::from_secs(10),
        }
    }
}

#[derive(Clone)]
pub struct GitHub {
    http: reqwest::Client,
    base: String,
    token: String,
    cache: PermissionCache,
    write_retry: WriteRetry,
    /// When fired, a write stops retrying and returns its last error.
    cancel: Option<CancellationToken>,
}

#[derive(Debug, thiserror::Error)]
pub enum GhError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("api error: {status}: {body}")]
    Api { status: u16, body: String },
    #[error("rate limited; remaining=0, reset_in={reset_in_secs}s")]
    RateLimited { reset_in_secs: i64 },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// A write's cancel token fired while it waited to retry; carries the
    /// error of its last attempt.
    #[error("cancelled while retrying; last error: {0}")]
    Cancelled(Box<GhError>),
}

impl GitHub {
    pub fn new(http: reqwest::Client, token: String) -> Self {
        Self {
            http,
            base: "https://api.github.com".into(),
            token,
            cache: PermissionCache::default(),
            write_retry: WriteRetry::default(),
            cancel: None,
        }
    }

    pub fn with_write_retry(mut self, retry: WriteRetry) -> Self {
        self.write_retry = retry;
        self
    }

    /// A client sharing this one's connection pool and token whose writes
    /// stop retrying once `cancel` fires.
    pub fn with_cancel(&self, cancel: CancellationToken) -> Self {
        let mut gh = self.clone();
        gh.cancel = Some(cancel);
        gh
    }

    /// Return a reference to the in-memory permission cache.
    pub fn perm_cache(&self) -> &PermissionCache {
        &self.cache
    }
    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<Response, GhError> {
        let url = format!("{}{}", self.base, path);
        let mut req = self
            .http
            .request(method.clone(), &url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "barry-dylan")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(b) = body {
            req = req.json(&b);
        }

        // Up to 3 attempts on 5xx / 429.
        let mut delay = Duration::from_millis(250);
        for attempt in 0..3u32 {
            let start = Instant::now();
            let resp = req.try_clone().expect("clonable request").send().await?;
            record_call(&method, start.elapsed());
            let status = resp.status();

            if status.is_success() {
                return Ok(resp);
            }
            if status.as_u16() == 429 || status.is_server_error() {
                if let Some(d) = parse_retry_after(resp.headers()) {
                    tokio::time::sleep(d).await;
                } else {
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(4);
                }
                if attempt < 2 {
                    continue;
                }
            }
            let headers = resp.headers().clone();
            let body = resp.text().await.unwrap_or_default();
            return Err(api_error(status.as_u16(), &headers, body));
        }
        unreachable!()
    }

    /// One attempt at a write, classified for [`GitHub::write`].
    async fn write_attempt(
        &self,
        method: &Method,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<Response, Failure> {
        let url = format!("{}{}", self.base, path);
        let start = Instant::now();
        let sent = self
            .http
            .request(method.clone(), &url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "barry-dylan")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(body)
            .send()
            .await;
        record_call(method, start.elapsed());
        let resp = match sent {
            Ok(r) => r,
            Err(e) => {
                // A refused or unresolvable connection sent nothing. Any other
                // transport error (a reset, a timeout, a dropped response) may
                // have come after GitHub received the request.
                let applied = if e.is_connect() {
                    Applied::No
                } else if e.is_builder() {
                    Applied::Never
                } else {
                    Applied::Unknown
                };
                return Err(Failure {
                    error: e.into(),
                    applied,
                    wait: None,
                });
            }
        };
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let headers = resp.headers().clone();
        let body = resp.text().await.unwrap_or_default();
        let code = status.as_u16();
        let retry_after = parse_retry_after(&headers);
        let (applied, wait) = if code == 429 {
            (Applied::No, retry_after)
        } else if code == 403 && is_secondary_rate_limit(&headers, &body) {
            // GitHub asks for at least a minute when it gives no Retry-After.
            (
                Applied::No,
                Some(retry_after.unwrap_or(Duration::from_secs(60))),
            )
        } else if status.is_server_error() {
            (Applied::Unknown, retry_after)
        } else {
            (Applied::Never, None)
        };
        Err(Failure {
            error: api_error(code, &headers, body),
            applied,
            wait,
        })
    }

    /// Send a write, retrying failures that may be transient.
    ///
    /// Retried: connection and other transport errors, 5xx, 429, and 403
    /// secondary rate limits. Any other 4xx, and a primary rate limit (which
    /// the dispatcher reschedules the job for), returns at once. Waits double
    /// from `base_delay` up to `max_delay` with jitter; a wait Retry-After
    /// sets is lengthened by up to a quarter so that writes limited together
    /// do not all retry together. A wait that would end past `deadline` is
    /// not started and the last error is returned. When `cancel` fires
    /// during a wait, the last error is returned inside
    /// [`GhError::Cancelled`], so that a rate limit met by a superseded
    /// review does not reschedule its job.
    ///
    /// When `landed` is `None` the write is safe to send twice. When it is
    /// `Some`, sending it twice would duplicate it; after a failure that may
    /// have been applied (a 5xx or a transport error other than a refused
    /// connection) it is not sent again until `landed` has twice reported
    /// that GitHub holds no copy of it, each lookup at least `lookup_settle`
    /// after the step before, since GitHub can show a write it has recorded
    /// a few seconds late. A copy it finds ends the write with that copy's
    /// id; a lookup that fails does not count and is repeated after the next
    /// wait.
    async fn write<F, Fut>(
        &self,
        op: &'static str,
        method: Method,
        path: &str,
        body: serde_json::Value,
        landed: Option<F>,
    ) -> Result<Written, GhError>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<Option<i64>, GhError>>,
    {
        let retry = self.write_retry;
        let started = Instant::now();
        let mut backoff = retry.base_delay;
        let mut last: Option<GhError> = None;
        // Lookups that must find nothing before the write is sent again.
        let mut lookups_owed = 0u32;
        loop {
            let mut wait = None;
            if lookups_owed > 0
                && let Some(landed) = &landed
            {
                match landed().await {
                    Ok(Some(id)) => {
                        tracing::info!(
                            op,
                            id,
                            "GitHub recorded a write that failed; not resending"
                        );
                        return Ok(Written::Found(id));
                    }
                    Ok(None) => lookups_owed -= 1,
                    Err(e) => {
                        tracing::warn!(op, error = %e, "could not check whether a failed write landed");
                    }
                }
            }
            if lookups_owed == 0 {
                match self.write_attempt(&method, path, &body).await {
                    Ok(resp) => return Ok(Written::Response(resp)),
                    Err(f) => {
                        match f.applied {
                            Applied::Never => {
                                metrics::counter!("barry_github_write_failed_total", "op" => op, "reason" => "permanent")
                                    .increment(1);
                                return Err(f.error);
                            }
                            Applied::No => {}
                            Applied::Unknown if landed.is_some() => {
                                lookups_owed = LOOKUPS_BEFORE_RESEND
                            }
                            Applied::Unknown => {}
                        }
                        wait = f.wait.map(after_retry_after);
                        last = Some(f.error);
                    }
                }
            }
            let last_err = last.as_ref().expect("a failed attempt precedes every wait");
            let mut wait = match wait {
                Some(w) => w,
                None => {
                    let w = jittered(backoff);
                    backoff = backoff.saturating_mul(2).min(retry.max_delay);
                    w
                }
            };
            if lookups_owed > 0 {
                wait = wait.max(retry.lookup_settle);
            }
            if started.elapsed() + wait > retry.deadline {
                tracing::warn!(op, error = %last_err, "GitHub write failed; giving up at the retry deadline");
                metrics::counter!("barry_github_write_failed_total", "op" => op, "reason" => "deadline")
                    .increment(1);
                return Err(last.expect("checked above"));
            }
            tracing::warn!(op, error = %last_err, wait_ms = wait.as_millis() as u64, "GitHub write failed; retrying");
            metrics::counter!("barry_github_write_retries_total", "op" => op).increment(1);
            let cancelled = match &self.cancel {
                Some(cancel) => tokio::select! {
                    _ = cancel.cancelled() => true,
                    _ = tokio::time::sleep(wait) => false,
                },
                None => {
                    tokio::time::sleep(wait).await;
                    false
                }
            };
            if cancelled {
                tracing::info!(op, "GitHub write cancelled while retrying");
                metrics::counter!("barry_github_write_failed_total", "op" => op, "reason" => "cancelled")
                    .increment(1);
                return Err(GhError::Cancelled(Box::new(last.expect("checked above"))));
            }
        }
    }

    /// A write that is safe to send twice, retried by [`GitHub::write`].
    ///
    /// GitHub has applied a write it answers with success, so a response
    /// body that cannot be read or parsed is logged and returned as `None`
    /// rather than as an error that would stop the caller's later writes.
    pub(crate) async fn write_json<T: DeserializeOwned>(
        &self,
        op: &'static str,
        method: Method,
        path: &str,
        body: serde_json::Value,
    ) -> Result<Option<T>, GhError> {
        type NoLookup = fn() -> std::future::Ready<Result<Option<i64>, GhError>>;
        match self.write(op, method, path, body, None::<NoLookup>).await? {
            Written::Response(r) => match r.json().await {
                Ok(v) => Ok(Some(v)),
                Err(e) => {
                    tracing::warn!(op, error = %e, "GitHub accepted a write; its response could not be read");
                    Ok(None)
                }
            },
            Written::Found(_) => unreachable!("no lookup was given"),
        }
    }

    /// A POST that creates something GitHub would hold two of if it were
    /// sent twice, retried by [`GitHub::write`]; `landed` finds a copy GitHub
    /// already holds. Returns the id of what was created.
    pub(crate) async fn post_unique<F, Fut>(
        &self,
        op: &'static str,
        path: &str,
        body: serde_json::Value,
        landed: F,
    ) -> Result<i64, GhError>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<Option<i64>, GhError>>,
    {
        #[derive(serde::Deserialize)]
        struct Created {
            id: i64,
        }
        match self
            .write(op, Method::POST, path, body, Some(landed))
            .await?
        {
            Written::Response(r) => Ok(r.json::<Created>().await?.id),
            Written::Found(id) => Ok(id),
        }
    }

    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, GhError> {
        let r = self.send(Method::GET, path, None).await?;
        Ok(r.json().await?)
    }

    pub async fn post_json<T: DeserializeOwned>(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<T, GhError> {
        let r = self.send(Method::POST, path, Some(body)).await?;
        Ok(r.json().await?)
    }

    pub async fn patch_json<T: DeserializeOwned>(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<T, GhError> {
        let r = self.send(Method::PATCH, path, Some(body)).await?;
        Ok(r.json().await?)
    }

    pub async fn graphql<T: DeserializeOwned>(
        &self,
        query: &str,
        vars: serde_json::Value,
    ) -> Result<T, GhError> {
        let body = serde_json::json!({ "query": query, "variables": vars });
        let r = self.send(Method::POST, "/graphql", Some(body)).await?;
        Ok(r.json().await?)
    }
}

/// Whether GitHub applied a write whose attempt failed.
enum Applied {
    /// It was refused before being applied; sending it again is safe.
    No,
    /// It may have been applied.
    Unknown,
    /// The failure is not transient; it is not retried.
    Never,
}

struct Failure {
    error: GhError,
    applied: Applied,
    /// How long GitHub asked to wait, in place of the backoff.
    wait: Option<Duration>,
}

enum Written {
    Response(Response),
    /// A copy GitHub already held, found after a failed attempt.
    Found(i64),
}

fn record_call(method: &Method, took: Duration) {
    // Extensions default to "EXTENSION".
    let method_str = match *method {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::PATCH => "PATCH",
        Method::DELETE => "DELETE",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        Method::TRACE => "TRACE",
        Method::CONNECT => "CONNECT",
        _ => "EXTENSION",
    };
    metrics::counter!("barry_github_api_calls_total", "method" => method_str).increment(1);
    metrics::histogram!("barry_github_api_duration_ms", "method" => method_str)
        .record(took.as_millis() as f64);
}

/// The error for a response GitHub did not answer with success. A 403 that
/// carries a rate-limit reset is reported as `RateLimited`, which the
/// dispatcher reschedules the job for.
fn api_error(status: u16, headers: &HeaderMap, body: String) -> GhError {
    if status == 403
        && let Some(reset) = parse_rate_reset(headers)
    {
        return GhError::RateLimited {
            reset_in_secs: reset,
        };
    }
    GhError::Api { status, body }
}

/// A 403 from GitHub's secondary rate limit, which lifts within minutes, as
/// opposed to the primary limit (`X-RateLimit-Remaining: 0`), which lifts at
/// the hourly reset.
fn is_secondary_rate_limit(h: &HeaderMap, body: &str) -> bool {
    h.contains_key("Retry-After") || body.contains("secondary rate limit")
}

/// A wait between half of `d` and `d`, so that writes failing together do
/// not retry together.
fn jittered(d: Duration) -> Duration {
    use std::hash::{BuildHasher, Hasher};
    let r = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    let half = d / 2;
    let nanos = half.as_nanos() as u64;
    half + Duration::from_nanos(if nanos == 0 { 0 } else { r % (nanos + 1) })
}

/// How many lookups must find nothing before a write that may have been
/// applied is sent again. See [`GitHub::write`].
const LOOKUPS_BEFORE_RESEND: u32 = 2;

/// A wait GitHub asked for, lengthened by up to a quarter so that tasks it
/// limited together do not all wake on the same second.
fn after_retry_after(d: Duration) -> Duration {
    d + jittered(d / 4)
}

fn parse_retry_after(h: &HeaderMap) -> Option<Duration> {
    h.get("Retry-After")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn parse_rate_reset(h: &HeaderMap) -> Option<i64> {
    let reset = h
        .get("X-RateLimit-Reset")
        .and_then(|v| v.to_str().ok())?
        .parse::<i64>()
        .ok()?;
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    Some((reset - now).max(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn retries_on_500_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .and(header("Authorization", "Bearer t"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": 1})))
            .mount(&server)
            .await;
        let gh = GitHub::new(reqwest::Client::new(), "t".into()).with_base(server.uri());
        let v: serde_json::Value = gh.get_json("/x").await.unwrap();
        assert_eq!(v["ok"], 1);
    }

    #[tokio::test]
    async fn surfaces_4xx_as_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        let gh = GitHub::new(reqwest::Client::new(), "t".into()).with_base(server.uri());
        let err = gh.get_json::<serde_json::Value>("/x").await.unwrap_err();
        assert!(matches!(err, GhError::Api { status: 404, .. }));
    }

    use crate::github::check_run::{CheckOutput, CheckRunInput, CheckStatus};

    /// Short enough that a test that runs out the deadline takes a second.
    fn fast_retry() -> WriteRetry {
        WriteRetry {
            deadline: Duration::from_secs(1),
            base_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(50),
            lookup_settle: Duration::from_millis(50),
        }
    }

    fn check_run() -> CheckRunInput {
        CheckRunInput {
            name: "barry/title".into(),
            head_sha: "h".into(),
            status: CheckStatus::Completed,
            conclusion: None,
            output: CheckOutput {
                title: "t".into(),
                summary: "s".into(),
                text: None,
            },
        }
    }

    async fn check_run_posts(server: &MockServer) -> usize {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/repos/o/r/check-runs")
            .count()
    }

    #[tokio::test]
    async fn a_check_run_that_fails_with_502_four_times_is_posted_on_the_fifth_attempt() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/check-runs"))
            .respond_with(ResponseTemplate::new(502))
            .up_to_n_times(4)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/check-runs"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 9})))
            .mount(&server)
            .await;
        let gh = GitHub::new(reqwest::Client::new(), "t".into())
            .with_base(server.uri())
            .with_write_retry(fast_retry());
        let r = gh.create_check_run("o", "r", &check_run()).await.unwrap();
        assert_eq!(r.unwrap().id, 9);
        assert_eq!(check_run_posts(&server).await, 5);
    }

    #[tokio::test]
    async fn a_write_rejected_with_422_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/check-runs"))
            .respond_with(ResponseTemplate::new(422).set_body_string("Unprocessable"))
            .expect(1)
            .mount(&server)
            .await;
        let gh = GitHub::new(reqwest::Client::new(), "t".into())
            .with_base(server.uri())
            .with_write_retry(fast_retry());
        let err = gh
            .create_check_run("o", "r", &check_run())
            .await
            .unwrap_err();
        assert!(matches!(err, GhError::Api { status: 422, .. }), "{err}");
    }

    #[tokio::test]
    async fn a_write_that_keeps_failing_returns_its_last_error_at_the_deadline() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/check-runs"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let gh = GitHub::new(reqwest::Client::new(), "t".into())
            .with_base(server.uri())
            .with_write_retry(fast_retry());
        let t = Instant::now();
        let err = gh
            .create_check_run("o", "r", &check_run())
            .await
            .unwrap_err();
        let took = t.elapsed();
        assert!(matches!(err, GhError::Api { status: 503, .. }), "{err}");
        assert!(took <= Duration::from_millis(1500), "took {took:?}");
        // 10, 20, 40 ms, then 50 ms waits: a 1 s deadline allows well over 4.
        assert!(check_run_posts(&server).await > 4);
    }

    #[tokio::test]
    async fn a_write_waits_as_long_as_retry_after_says() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/check-runs"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/check-runs"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 9})))
            .mount(&server)
            .await;
        let gh = GitHub::new(reqwest::Client::new(), "t".into())
            .with_base(server.uri())
            .with_write_retry(WriteRetry {
                deadline: Duration::from_secs(5),
                ..fast_retry()
            });
        let t = Instant::now();
        gh.create_check_run("o", "r", &check_run()).await.unwrap();
        assert!(t.elapsed() >= Duration::from_secs(1), "{:?}", t.elapsed());
        assert_eq!(check_run_posts(&server).await, 2);
    }

    #[tokio::test]
    async fn a_secondary_rate_limit_is_retried_after_its_retry_after() {
        let server = MockServer::start().await;
        let reset = time::OffsetDateTime::now_utc().unix_timestamp() + 3600;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/check-runs"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("Retry-After", "1")
                    .insert_header("X-RateLimit-Remaining", "4000")
                    .insert_header("X-RateLimit-Reset", reset.to_string().as_str())
                    .set_body_string(r#"{"message":"You have exceeded a secondary rate limit."}"#),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/check-runs"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 9})))
            .mount(&server)
            .await;
        let gh = GitHub::new(reqwest::Client::new(), "t".into())
            .with_base(server.uri())
            .with_write_retry(WriteRetry {
                deadline: Duration::from_secs(5),
                ..fast_retry()
            });
        let t = Instant::now();
        gh.create_check_run("o", "r", &check_run()).await.unwrap();
        assert!(t.elapsed() >= Duration::from_secs(1), "{:?}", t.elapsed());
        assert_eq!(check_run_posts(&server).await, 2);
    }

    #[tokio::test]
    async fn a_retry_after_past_the_deadline_returns_the_rate_limit_without_waiting() {
        let server = MockServer::start().await;
        let reset = time::OffsetDateTime::now_utc().unix_timestamp() + 3600;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/check-runs"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("Retry-After", "60")
                    .insert_header("X-RateLimit-Reset", reset.to_string().as_str())
                    .set_body_string(r#"{"message":"You have exceeded a secondary rate limit."}"#),
            )
            .expect(1)
            .mount(&server)
            .await;
        let gh = GitHub::new(reqwest::Client::new(), "t".into())
            .with_base(server.uri())
            .with_write_retry(fast_retry());
        let t = Instant::now();
        let err = gh
            .create_check_run("o", "r", &check_run())
            .await
            .unwrap_err();
        assert!(matches!(err, GhError::RateLimited { .. }), "{err}");
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    }

    #[tokio::test]
    async fn a_cancelled_write_stops_retrying() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/check-runs"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;
        let cancel = CancellationToken::new();
        let gh = GitHub::new(reqwest::Client::new(), "t".into())
            .with_base(server.uri())
            .with_write_retry(WriteRetry {
                deadline: Duration::from_secs(60),
                ..fast_retry()
            })
            .with_cancel(cancel.clone());
        let fire = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            fire.cancel();
        });
        let t = Instant::now();
        let err = gh
            .create_check_run("o", "r", &check_run())
            .await
            .unwrap_err();
        match &err {
            GhError::Cancelled(last) => {
                assert!(matches!(**last, GhError::Api { status: 502, .. }), "{err}")
            }
            _ => panic!("expected Cancelled, got {err}"),
        }
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    }

    #[test]
    fn a_wait_github_asked_for_is_lengthened_by_up_to_a_quarter() {
        let asked = Duration::from_secs(60);
        let waits: Vec<Duration> = (0..50).map(|_| after_retry_after(asked)).collect();
        for w in &waits {
            assert!(*w >= asked && *w <= asked + asked / 4, "{w:?}");
        }
        assert!(waits.iter().any(|w| *w > asked), "no wait was lengthened");
        assert!(
            waits.iter().any(|w| *w != waits[0]),
            "every task would wake together"
        );
    }
}
