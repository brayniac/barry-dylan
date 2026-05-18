use crate::storage::Store;
use crate::storage::actor::{ActorCommand, Reply};
use tokio::sync::oneshot;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CachedInstallation {
    /// Positive cache entry: App is installed; installation_id is known.
    Cached { installation_id: i64 },
    /// Negative cache entry: App is not installed on this owner.
    /// Returned only if the entry is within the 1h TTL.
    NotInstalled,
}

impl Store {
    /// Look up a cached installation entry for (identity, owner).
    /// Negative cache entries older than 1h return None (treat as miss).
    pub async fn get_installation(
        &self,
        identity: &str,
        owner: &str,
        now_ts: i64,
    ) -> anyhow::Result<Option<CachedInstallation>> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::GetInstallation {
                identity: identity.to_string(),
                owner: owner.to_string(),
                now_ts,
                reply: Reply { tx },
            })
            .map_err(|_| crate::storage::DbError::Closed)?;
        let result = rx.await.map_err(|_| crate::storage::DbError::Closed)??;
        Ok(result)
    }

    /// Insert or update an installation cache entry. Pass `None` for `installation_id`
    /// to store a negative cache entry.
    pub async fn put_installation(
        &self,
        identity: &str,
        owner: &str,
        installation_id: Option<i64>,
        now_ts: i64,
    ) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::PutInstallation {
                identity: identity.to_string(),
                owner: owner.to_string(),
                installation_id,
                cached_at: now_ts,
                reply: Reply { tx },
            })
            .map_err(|_| crate::storage::DbError::Closed)?;
        rx.await.map_err(|_| crate::storage::DbError::Closed)??;
        Ok(())
    }

    /// Delete any cached entry for (identity, owner). Used when a token mint
    /// returns 401 (App was uninstalled after we cached the positive entry).
    pub async fn invalidate_installation(
        &self,
        identity: &str,
        owner: &str,
    ) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::InvalidateInstallation {
                identity: identity.to_string(),
                owner: owner.to_string(),
                reply: Reply { tx },
            })
            .map_err(|_| crate::storage::DbError::Closed)?;
        rx.await.map_err(|_| crate::storage::DbError::Closed)??;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn positive_entry_round_trip() {
        let s = Store::in_memory().await.unwrap();
        s.put_installation("other_barry", "acme", Some(42), 1000)
            .await
            .unwrap();
        let v = s
            .get_installation("other_barry", "acme", 1000)
            .await
            .unwrap();
        assert_eq!(
            v,
            Some(CachedInstallation::Cached {
                installation_id: 42
            })
        );
    }

    #[tokio::test]
    async fn negative_entry_within_ttl() {
        let s = Store::in_memory().await.unwrap();
        s.put_installation("other_barry", "acme", None, 1000)
            .await
            .unwrap();
        let v = s
            .get_installation("other_barry", "acme", 1500)
            .await
            .unwrap();
        assert_eq!(v, Some(CachedInstallation::NotInstalled));
    }

    #[tokio::test]
    async fn negative_entry_stale_returns_none() {
        let s = Store::in_memory().await.unwrap();
        s.put_installation("other_barry", "acme", None, 1000)
            .await
            .unwrap();
        // now > cached_at + 3600
        let v = s
            .get_installation("other_barry", "acme", 5000)
            .await
            .unwrap();
        assert_eq!(v, None);
    }

    #[tokio::test]
    async fn positive_entry_never_expires() {
        let s = Store::in_memory().await.unwrap();
        s.put_installation("other_barry", "acme", Some(7), 0)
            .await
            .unwrap();
        let v = s
            .get_installation("other_barry", "acme", 100_000)
            .await
            .unwrap();
        assert_eq!(
            v,
            Some(CachedInstallation::Cached { installation_id: 7 })
        );
    }

    #[tokio::test]
    async fn identities_do_not_collide() {
        let s = Store::in_memory().await.unwrap();
        s.put_installation("barry", "acme", Some(1), 1000)
            .await
            .unwrap();
        s.put_installation("other_barry", "acme", None, 1000)
            .await
            .unwrap();

        let b = s.get_installation("barry", "acme", 1000).await.unwrap();
        let ob = s
            .get_installation("other_barry", "acme", 1000)
            .await
            .unwrap();

        assert_eq!(
            b,
            Some(CachedInstallation::Cached { installation_id: 1 })
        );
        assert_eq!(ob, Some(CachedInstallation::NotInstalled));
    }

    #[tokio::test]
    async fn invalidate_removes_row() {
        let s = Store::in_memory().await.unwrap();
        s.put_installation("other_barry", "acme", Some(42), 1000)
            .await
            .unwrap();
        s.invalidate_installation("other_barry", "acme")
            .await
            .unwrap();
        let v = s
            .get_installation("other_barry", "acme", 1000)
            .await
            .unwrap();
        assert_eq!(v, None);
    }
}
