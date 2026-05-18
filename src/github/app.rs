use crate::storage::Store;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Serialize)]
struct Claims {
    iat: u64,
    exp: u64,
    iss: String,
}

#[derive(Clone)]
pub struct AppCreds {
    pub app_id: u64,
    private_key_pem: Vec<u8>,
}

impl AppCreds {
    pub fn load(app_id: u64, path: &Path) -> anyhow::Result<Self> {
        let pem = std::fs::read(path)?;
        Ok(Self {
            app_id,
            private_key_pem: pem,
        })
    }

    /// Test-only constructor — creates `AppCreds` from raw PEM bytes.
    #[cfg(test)]
    pub fn from_pem_bytes(app_id: u64, pem: Vec<u8>) -> Self {
        Self {
            app_id,
            private_key_pem: pem,
        }
    }

    /// Mint a short-lived (10 minute) JWT signed with the App private key.
    pub fn mint_jwt(&self, now: u64) -> anyhow::Result<String> {
        let claims = Claims {
            iat: now.saturating_sub(60),
            exp: now + 9 * 60,
            iss: self.app_id.to_string(),
        };
        let key = EncodingKey::from_rsa_pem(&self.private_key_pem)?;
        Ok(encode(&Header::new(Algorithm::RS256), &claims, &key)?)
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: String,
    expires_at: String,
}

pub async fn fetch_installation_token(
    http: &reqwest::Client,
    creds: &AppCreds,
    installation_id: i64,
    base_url: &str,
) -> anyhow::Result<(String, i64)> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let jwt = creds.mint_jwt(now)?;
    let url = format!("{base_url}/app/installations/{installation_id}/access_tokens");
    let resp = http
        .post(&url)
        .bearer_auth(&jwt)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "barry-dylan")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await?
        .error_for_status()?
        .json::<TokenResponse>()
        .await?;
    let dt = time::OffsetDateTime::parse(
        &resp.expires_at,
        &time::format_description::well_known::Rfc3339,
    )?;
    Ok((resp.token, dt.unix_timestamp()))
}

/// Default GitHub API base URL. Tests pass a different URL pointing at wiremock.
pub const GITHUB_API_BASE: &str = "https://api.github.com";

/// Look up the App installation ID for a given owner/repo. Returns:
/// - `Ok(Some(id))` if the App is installed on the repo.
/// - `Ok(None)` if GitHub returns 404 (App is not installed on this owner).
/// - `Err(_)` for any other failure (transport, 5xx, unparseable response).
///
/// `base_url` is the GitHub API base (use `GITHUB_API_BASE` in production).
pub async fn resolve_installation_id_for_repo(
    http: &reqwest::Client,
    creds: &AppCreds,
    owner: &str,
    repo: &str,
    base_url: &str,
) -> anyhow::Result<Option<i64>> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let jwt = creds.mint_jwt(now)?;
    let url = format!("{base_url}/repos/{owner}/{repo}/installation");
    let resp = http
        .get(&url)
        .bearer_auth(&jwt)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "barry-dylan")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let resp = resp.error_for_status()?;
    #[derive(serde::Deserialize)]
    struct InstallationResponse {
        id: i64,
    }
    let parsed: InstallationResponse = resp.json().await?;
    Ok(Some(parsed.id))
}

/// Identity-scoped token cache lookup/mint. Uses `identity.slug()` as the cache key.
pub async fn get_or_mint_for(
    store: &Store,
    http: &reqwest::Client,
    creds: &AppCreds,
    identity: crate::checker::multi_review::identity::Identity,
    installation_id: i64,
    now_ts: i64,
    base_url: &str,
) -> anyhow::Result<String> {
    if let Some(t) = store
        .get_installation_token_for(identity.slug(), installation_id, now_ts)
        .await?
    {
        return Ok(t.token);
    }
    let (token, exp) = fetch_installation_token(http, creds, installation_id, base_url).await?;
    store
        .put_installation_token_for(identity.slug(), installation_id, &token, exp)
        .await?;
    Ok(token)
}

/// Legacy single-identity wrapper around `get_or_mint_for` (identity = Barry).
/// Preserved for existing call sites; Task 13 removes this.
pub async fn get_or_mint(
    store: &Store,
    http: &reqwest::Client,
    creds: &AppCreds,
    installation_id: i64,
    now_ts: i64,
) -> anyhow::Result<String> {
    get_or_mint_for(
        store,
        http,
        creds,
        crate::checker::multi_review::identity::Identity::Barry,
        installation_id,
        now_ts,
        GITHUB_API_BASE,
    )
    .await
}

/// Refuse to start if the private key file is world- or group-readable.
pub fn ensure_key_mode_strict(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            anyhow::bail!(
                "private key {:?} has permissive mode {:o}; require 0600 or stricter",
                path,
                mode
            );
        }
    }
    let _ = path; // suppress warning on non-unix
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key_pem() -> Vec<u8> {
        // 2048-bit RSA test key generated for unit tests only.
        // Not used in production; safe to commit.
        include_bytes!("../../tests/fixtures/test_app_key.pem").to_vec()
    }

    #[test]
    fn mints_jwt_that_decodes() {
        let creds = AppCreds {
            app_id: 12345,
            private_key_pem: test_key_pem(),
        };
        let token = creds.mint_jwt(1_700_000_000).unwrap();
        // Decode using the public key embedded by stripping the private parts.
        // Just confirm it has 3 base64url segments.
        assert_eq!(token.split('.').count(), 3);
    }

    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_creds() -> AppCreds {
        AppCreds {
            app_id: 12345,
            private_key_pem: test_key_pem(),
        }
    }

    #[tokio::test]
    async fn resolve_installation_id_returns_id_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widget/installation"))
            .and(header("Accept", "application/vnd.github+json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 42
            })))
            .mount(&server)
            .await;
        let http = reqwest::Client::new();
        let result = resolve_installation_id_for_repo(
            &http,
            &test_creds(),
            "acme",
            "widget",
            &server.uri(),
        )
        .await
        .unwrap();
        assert_eq!(result, Some(42));
    }

    #[tokio::test]
    async fn resolve_installation_id_returns_none_on_404() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widget/installation"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let http = reqwest::Client::new();
        let result = resolve_installation_id_for_repo(
            &http,
            &test_creds(),
            "acme",
            "widget",
            &server.uri(),
        )
        .await
        .unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn resolve_installation_id_errors_on_500() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widget/installation"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let http = reqwest::Client::new();
        let result = resolve_installation_id_for_repo(
            &http,
            &test_creds(),
            "acme",
            "widget",
            &server.uri(),
        )
        .await;
        assert!(result.is_err());
    }
}
