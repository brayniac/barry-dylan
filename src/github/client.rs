use reqwest::header::HeaderMap;
use reqwest::{Method, Response};
use serde::de::DeserializeOwned;
use std::time::Duration;

#[derive(Clone)]
pub struct GitHub {
    http: reqwest::Client,
    base: String,
    token: String,
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
}

impl GitHub {
    pub fn new(http: reqwest::Client, token: String) -> Self {
        Self { http, base: "https://api.github.com".into(), token }
    }
    pub fn with_base(mut self, base: impl Into<String>) -> Self { self.base = base.into(); self }

    async fn send(&self, method: Method, path: &str, body: Option<serde_json::Value>) -> Result<Response, GhError> {
        let url = format!("{}{}", self.base, path);
        let mut req = self.http.request(method.clone(), &url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "barry-dylan")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(b) = body { req = req.json(&b); }

        // Up to 3 attempts on 5xx / 429.
        let mut delay = Duration::from_millis(250);
        for attempt in 0..3u32 {
            let resp = req.try_clone().expect("clonable request").send().await?;
            let status = resp.status();
            if status.is_success() { return Ok(resp); }
            if status.as_u16() == 429 || status.is_server_error() {
                if let Some(d) = parse_retry_after(resp.headers()) {
                    tokio::time::sleep(d).await;
                } else {
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(4);
                }
                if attempt < 2 { continue; }
            }
            // Secondary rate limit / hard limit.
            if status.as_u16() == 403 {
                if let Some(reset) = parse_rate_reset(resp.headers()) {
                    return Err(GhError::RateLimited { reset_in_secs: reset });
                }
            }
            let body = resp.text().await.unwrap_or_default();
            return Err(GhError::Api { status: status.as_u16(), body });
        }
        unreachable!()
    }

    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, GhError> {
        let r = self.send(Method::GET, path, None).await?;
        Ok(r.json().await?)
    }

    pub async fn post_json<T: DeserializeOwned>(&self, path: &str, body: serde_json::Value) -> Result<T, GhError> {
        let r = self.send(Method::POST, path, Some(body)).await?;
        Ok(r.json().await?)
    }

    pub async fn patch_json<T: DeserializeOwned>(&self, path: &str, body: serde_json::Value) -> Result<T, GhError> {
        let r = self.send(Method::PATCH, path, Some(body)).await?;
        Ok(r.json().await?)
    }

    pub async fn graphql<T: DeserializeOwned>(&self, query: &str, vars: serde_json::Value) -> Result<T, GhError> {
        let body = serde_json::json!({ "query": query, "variables": vars });
        let r = self.send(Method::POST, "/graphql", Some(body)).await?;
        Ok(r.json().await?)
    }
}

fn parse_retry_after(h: &HeaderMap) -> Option<Duration> {
    h.get("Retry-After")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn parse_rate_reset(h: &HeaderMap) -> Option<i64> {
    let reset = h.get("X-RateLimit-Reset").and_then(|v| v.to_str().ok())?.parse::<i64>().ok()?;
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
        Mock::given(method("GET")).and(path("/x"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/x")).and(header("Authorization", "Bearer t"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": 1})))
            .mount(&server).await;
        let gh = GitHub::new(reqwest::Client::new(), "t".into()).with_base(&server.uri());
        let v: serde_json::Value = gh.get_json("/x").await.unwrap();
        assert_eq!(v["ok"], 1);
    }

    #[tokio::test]
    async fn surfaces_4xx_as_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/x"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server).await;
        let gh = GitHub::new(reqwest::Client::new(), "t".into()).with_base(&server.uri());
        let err = gh.get_json::<serde_json::Value>("/x").await.unwrap_err();
        assert!(matches!(err, GhError::Api { status: 404, .. }));
    }
}
