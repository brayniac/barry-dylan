use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// In-memory registry mapping each active PR job to its cancellation token.
///
/// When a `pull_request.closed` event arrives, `cancel()` fires the token for
/// that PR. Any checker task racing `select!(run | cancelled)` will then abort
/// without posting results.
#[derive(Clone, Default)]
pub struct CancelRegistry {
    inner: Arc<Mutex<HashMap<(String, String, i64), CancellationToken>>>,
}

impl CancelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a fresh token for this PR's in-flight job. Returns the token
    /// for the caller to pass into the checker loop.
    pub async fn register(&self, owner: &str, repo: &str, pr: i64) -> CancellationToken {
        let token = CancellationToken::new();
        let mut map = self.inner.lock().await;
        if let Some(old) = map.insert((owner.into(), repo.into(), pr), token.clone()) {
            // Defensive: cancel any stale token if a previous job didn't clean up.
            old.cancel();
        }
        token
    }

    /// Cancel (and remove) the token for this PR. Called by `handle_pr_closed`.
    pub async fn cancel(&self, owner: &str, repo: &str, pr: i64) {
        if let Some(token) = self
            .inner
            .lock()
            .await
            .remove(&(owner.into(), repo.into(), pr))
        {
            token.cancel();
        }
    }

    /// Remove the token when a job completes normally (no cancellation needed).
    pub async fn remove(&self, owner: &str, repo: &str, pr: i64) {
        self.inner
            .lock()
            .await
            .remove(&(owner.into(), repo.into(), pr));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_returns_uncancelled_token() {
        let reg = CancelRegistry::new();
        let token = reg.register("o", "r", 1).await;
        assert!(!token.is_cancelled());
    }

    #[tokio::test]
    async fn cancel_fires_registered_token() {
        let reg = CancelRegistry::new();
        let token = reg.register("o", "r", 1).await;
        reg.cancel("o", "r", 1).await;
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn cancel_noop_when_no_token() {
        let reg = CancelRegistry::new();
        reg.cancel("o", "r", 99).await; // must not panic
    }

    #[tokio::test]
    async fn remove_cleans_up_without_cancelling() {
        let reg = CancelRegistry::new();
        let token = reg.register("o", "r", 1).await;
        reg.remove("o", "r", 1).await;
        assert!(!token.is_cancelled());
        // Cancelling a non-existent entry is a no-op.
        reg.cancel("o", "r", 1).await;
        assert!(!token.is_cancelled());
    }

    #[tokio::test]
    async fn tokens_are_per_pr() {
        let reg = CancelRegistry::new();
        let t1 = reg.register("o", "r", 1).await;
        let t2 = reg.register("o", "r", 2).await;
        reg.cancel("o", "r", 1).await;
        assert!(t1.is_cancelled());
        assert!(!t2.is_cancelled());
    }
}
