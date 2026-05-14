use crate::storage::Store;
use sqlx::Row;

#[derive(Debug, Clone)]
pub struct CachedToken {
    pub token: String,
    pub expires_at: i64,
}

impl Store {
    /// Identity-scoped token lookup. `identity` is the slug (e.g. `"barry"`, `"other_barry"`).
    pub async fn get_installation_token_for(
        &self,
        identity: &str,
        installation_id: i64,
        now_ts: i64,
    ) -> anyhow::Result<Option<CachedToken>> {
        let row = sqlx::query(
            "SELECT token, expires_at FROM installation_tokens \
             WHERE installation_id = ?1 AND identity = ?2",
        )
        .bind(installation_id)
        .bind(identity)
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

    /// Identity-scoped token store. `identity` is the slug (e.g. `"barry"`, `"other_barry"`).
    pub async fn put_installation_token_for(
        &self,
        identity: &str,
        installation_id: i64,
        token: &str,
        expires_at: i64,
    ) -> anyhow::Result<()> {
        sqlx::query(
            r#"INSERT INTO installation_tokens (installation_id, identity, token, expires_at)
               VALUES (?1, ?2, ?3, ?4)
               ON CONFLICT(installation_id, identity) DO UPDATE SET token=excluded.token, expires_at=excluded.expires_at"#,
        )
        .bind(installation_id)
        .bind(identity)
        .bind(token)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ── Legacy wrappers (identity = "barry") ──────────────────────────────────
    // Preserved for existing call sites; Task 13 removes them.

    pub async fn get_installation_token(
        &self,
        installation_id: i64,
        now_ts: i64,
    ) -> anyhow::Result<Option<CachedToken>> {
        self.get_installation_token_for("barry", installation_id, now_ts)
            .await
    }

    pub async fn put_installation_token(
        &self,
        installation_id: i64,
        token: &str,
        expires_at: i64,
    ) -> anyhow::Result<()> {
        self.put_installation_token_for("barry", installation_id, token, expires_at)
            .await
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

    #[tokio::test]
    async fn identity_scoped_tokens_do_not_collide() {
        let s = Store::in_memory().await.unwrap();
        // Same installation_id, two different identities.
        s.put_installation_token_for("barry", 99, "tok-barry", 9999)
            .await
            .unwrap();
        s.put_installation_token_for("other_barry", 99, "tok-other", 9999)
            .await
            .unwrap();

        let tb = s
            .get_installation_token_for("barry", 99, 1000)
            .await
            .unwrap()
            .unwrap();
        let to = s
            .get_installation_token_for("other_barry", 99, 1000)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(tb.token, "tok-barry");
        assert_eq!(to.token, "tok-other");
        // Ensure they are independent.
        assert_ne!(tb.token, to.token);
    }

    #[tokio::test]
    async fn identity_scoped_token_respects_expiry() {
        let s = Store::in_memory().await.unwrap();
        // Expires at 1030; with 60s skew margin, should be stale at now=1000.
        s.put_installation_token_for("other_barry", 7, "tok", 1030)
            .await
            .unwrap();
        let result = s
            .get_installation_token_for("other_barry", 7, 1000)
            .await
            .unwrap();
        assert!(result.is_none());
    }
}
