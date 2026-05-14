use crate::storage::Store;
use crate::storage::actor::{ActorCommand, Reply};
use tokio::sync::oneshot;

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
        // Check cache first.
        if let Some(token) = self.cache.get(identity, installation_id, now_ts) {
            return Ok(Some(token));
        }

        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::GetTokenFor {
                identity: identity.to_string(),
                installation_id,
                now_ts,
                reply: Reply { tx },
            })
            .map_err(|_| crate::storage::DbError::Closed)?;
        let result = rx.await.map_err(|_| crate::storage::DbError::Closed)??;

        // Write-through cache on hit.
        if let Some(token) = &result {
            self.cache.put(identity, installation_id, token.clone());
        }

        Ok(result)
    }

    /// Identity-scoped token store. `identity` is the slug (e.g. `"barry"`, `"other_barry"`).
    pub async fn put_installation_token_for(
        &self,
        identity: &str,
        installation_id: i64,
        token: &str,
        expires_at: i64,
    ) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::PutTokenFor {
                identity: identity.to_string(),
                installation_id,
                token: token.to_string(),
                expires_at,
                reply: Reply { tx },
            })
            .map_err(|_| crate::storage::DbError::Closed)?;
        rx.await.map_err(|_| crate::storage::DbError::Closed)??;

        // Invalidate cache on successful write.
        self.cache.invalidate(identity, installation_id);

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
