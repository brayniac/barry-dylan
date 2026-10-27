use crate::storage::Store;
use crate::storage::actor::{ActorCommand, Reply};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

/// How long an accepted delivery is remembered. A copy arriving after this
/// is accepted again. GitHub keeps deliveries available for redelivery for 3
/// days, so a week covers every redelivery GitHub itself can make.
pub const DELIVERY_RETENTION_SECS: i64 = 7 * 24 * 60 * 60;

/// How old an event may be and still be acted on, by the timestamp in its
/// own signed payload. Shorter than [`DELIVERY_RETENTION_SECS`], so a
/// delivery is refused as stale before its row can be pruned and a copy of
/// it accepted again. The day between the two absorbs clock skew between
/// GitHub and barry.
pub const STALE_AFTER_SECS: i64 = DELIVERY_RETENTION_SECS - 24 * 60 * 60;
const _: () = assert!(STALE_AFTER_SECS < DELIVERY_RETENTION_SECS);

/// The key a delivery is remembered by: the lowercase hex SHA-256 of its raw
/// body. The webhook signature covers the body and no header, so this is the
/// part of a delivery a copy cannot change while staying validly signed.
pub fn body_sha256(body: &[u8]) -> String {
    Sha256::digest(body)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl Store {
    /// Record the delivery whose body hashes to `body_sha256` as accepted at
    /// `now_ts`. Returns `true` if that body has not been seen within the
    /// retention window, `false` if it has. Check and insert are one actor
    /// command, so two copies arriving together cannot both get `true`.
    /// Deliveries older than [`DELIVERY_RETENTION_SECS`] are pruned on the
    /// way. `delivery_id` is stored alongside for reading the table.
    pub async fn claim_delivery(
        &self,
        body_sha256: &str,
        delivery_id: &str,
        now_ts: i64,
    ) -> anyhow::Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::ClaimDelivery {
                body_sha256: body_sha256.to_string(),
                delivery_id: delivery_id.to_string(),
                now_ts,
                prune_before: now_ts - DELIVERY_RETENTION_SECS,
                reply: Reply { tx },
            })
            .map_err(|_| crate::storage::DbError::Closed)?;
        let fresh = rx.await.map_err(|_| crate::storage::DbError::Closed)??;
        Ok(fresh)
    }

    /// Forget the delivery recorded under `body_sha256`, so that a later copy
    /// of it is accepted. Used when a claimed delivery could not be acted on.
    pub async fn release_delivery(&self, body_sha256: &str) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::ReleaseDelivery {
                body_sha256: body_sha256.to_string(),
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
    async fn a_body_is_claimed_once_whatever_its_delivery_id() {
        let s = Store::in_memory().await.unwrap();
        let a = body_sha256(b"{\"action\":\"opened\"}");
        let b = body_sha256(b"{\"action\":\"closed\"}");
        assert!(s.claim_delivery(&a, "d1", 1000).await.unwrap());
        assert!(!s.claim_delivery(&a, "d2", 1001).await.unwrap());
        assert!(s.claim_delivery(&b, "d1", 1001).await.unwrap());
    }

    #[test]
    fn the_body_hash_is_lowercase_hex_sha256() {
        // sha256("abc"), FIPS 180-2 appendix B.1.
        assert_eq!(
            body_sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[tokio::test]
    async fn a_released_delivery_can_be_claimed_again() {
        let s = Store::in_memory().await.unwrap();
        assert!(s.claim_delivery("d1", "id", 1000).await.unwrap());
        s.release_delivery("d1").await.unwrap();
        assert!(s.claim_delivery("d1", "id", 1001).await.unwrap());
    }

    #[tokio::test]
    async fn deliveries_past_the_retention_are_pruned() {
        let s = Store::in_memory().await.unwrap();
        assert!(s.claim_delivery("old", "id", 1000).await.unwrap());
        assert!(s.claim_delivery("recent", "id", 2000).await.unwrap());

        // One second past the retention for "old", inside it for "recent".
        let later = 1000 + DELIVERY_RETENTION_SECS + 1;
        assert!(s.claim_delivery("new", "id", later).await.unwrap());
        assert_eq!(s.count_rows("seen_deliveries").await.unwrap(), 2);
        assert!(s.claim_delivery("old", "id", later).await.unwrap());
        assert!(!s.claim_delivery("recent", "id", later).await.unwrap());
    }

    #[tokio::test]
    async fn an_existing_database_gains_the_delivery_table() {
        // A database written before `seen_deliveries` existed: `jobs` is
        // there, the new table is not. Opening it must create the table.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        {
            let pool = sqlx::pool::PoolOptions::<sqlx::Sqlite>::new()
                .max_connections(1)
                .connect_with(
                    sqlx::sqlite::SqliteConnectOptions::new()
                        .filename(&path)
                        .create_if_missing(true),
                )
                .await
                .unwrap();
            sqlx::query(
                "CREATE TABLE jobs (id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 installation_id INTEGER NOT NULL, repo_owner TEXT NOT NULL, \
                 repo_name TEXT NOT NULL, pr_number INTEGER NOT NULL, \
                 event_kind TEXT NOT NULL, delivery_id TEXT NOT NULL, \
                 received_at INTEGER NOT NULL, run_after INTEGER NOT NULL, \
                 leased_until INTEGER, attempts INTEGER NOT NULL DEFAULT 0, \
                 last_error TEXT)",
            )
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }
        let s = Store::open(&path).await.unwrap();
        assert!(s.claim_delivery("d1", "id", 1000).await.unwrap());
        assert!(!s.claim_delivery("d1", "id", 1001).await.unwrap());
    }
}
