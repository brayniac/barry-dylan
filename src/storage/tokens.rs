use crate::storage::Store;
use sqlx::Row;

#[derive(Debug, Clone)]
pub struct CachedToken {
    pub token: String,
    pub expires_at: i64,
}

impl Store {
    pub async fn get_installation_token(
        &self,
        installation_id: i64,
        now_ts: i64,
    ) -> anyhow::Result<Option<CachedToken>> {
        let row = sqlx::query(
            "SELECT token, expires_at FROM installation_tokens WHERE installation_id = ?1",
        )
        .bind(installation_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|r| {
            let token: String = r.get("token");
            let expires_at: i64 = r.get("expires_at");
            // 60s skew margin.
            if expires_at - 60 > now_ts {
                Some(CachedToken { token, expires_at })
            } else {
                None
            }
        }))
    }

    pub async fn put_installation_token(
        &self,
        installation_id: i64,
        token: &str,
        expires_at: i64,
    ) -> anyhow::Result<()> {
        sqlx::query(
            r#"INSERT INTO installation_tokens (installation_id, token, expires_at)
               VALUES (?1, ?2, ?3)
               ON CONFLICT(installation_id) DO UPDATE SET token=excluded.token, expires_at=excluded.expires_at"#,
        )
        .bind(installation_id).bind(token).bind(expires_at)
        .execute(&self.pool).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn store_and_fetch_token() {
        let s = Store::in_memory().await.unwrap();
        s.put_installation_token(42, "tok", 2000).await.unwrap();
        let t = s.get_installation_token(42, 1000).await.unwrap().unwrap();
        assert_eq!(t.token, "tok");
    }

    #[tokio::test]
    async fn returns_none_when_expiring_soon() {
        let s = Store::in_memory().await.unwrap();
        s.put_installation_token(42, "tok", 1030).await.unwrap();
        let t = s.get_installation_token(42, 1000).await.unwrap();
        assert!(t.is_none()); // expires_at - 60 = 970 < now=1000 → expired
    }
}
