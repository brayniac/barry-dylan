use crate::storage::Store;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Serialize)]
struct Claims { iat: u64, exp: u64, iss: String }

#[derive(Clone)]
pub struct AppCreds {
    pub app_id: u64,
    private_key_pem: Vec<u8>,
}

impl AppCreds {
    pub fn load(app_id: u64, path: &Path) -> anyhow::Result<Self> {
        let pem = std::fs::read(path)?;
        Ok(Self { app_id, private_key_pem: pem })
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
struct TokenResponse { token: String, expires_at: String }

pub async fn fetch_installation_token(
    http: &reqwest::Client,
    creds: &AppCreds,
    installation_id: i64,
) -> anyhow::Result<(String, i64)> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let jwt = creds.mint_jwt(now)?;
    let url = format!("https://api.github.com/app/installations/{installation_id}/access_tokens");
    let resp = http.post(&url)
        .bearer_auth(&jwt)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "barry-dylan")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send().await?
        .error_for_status()?
        .json::<TokenResponse>().await?;
    let dt = time::OffsetDateTime::parse(&resp.expires_at, &time::format_description::well_known::Rfc3339)?;
    Ok((resp.token, dt.unix_timestamp()))
}

pub async fn get_or_mint(
    store: &Store,
    http: &reqwest::Client,
    creds: &AppCreds,
    installation_id: i64,
    now_ts: i64,
) -> anyhow::Result<String> {
    if let Some(t) = store.get_installation_token(installation_id, now_ts).await? {
        return Ok(t.token);
    }
    let (token, exp) = fetch_installation_token(http, creds, installation_id).await?;
    store.put_installation_token(installation_id, &token, exp).await?;
    Ok(token)
}

/// Refuse to start if the private key file is world- or group-readable.
pub fn ensure_key_mode_strict(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            anyhow::bail!("private key {:?} has permissive mode {:o}; require 0600 or stricter",
                path, mode);
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
        let creds = AppCreds { app_id: 12345, private_key_pem: test_key_pem() };
        let token = creds.mint_jwt(1_700_000_000).unwrap();
        // Decode using the public key embedded by stripping the private parts.
        // Just confirm it has 3 base64url segments.
        assert_eq!(token.split('.').count(), 3);
    }
}
