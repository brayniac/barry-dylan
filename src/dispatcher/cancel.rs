use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

type Key = (String, String, i64);

/// A PR's review in flight: its token, the head SHA it is reviewing, the
/// signal that asks it to re-check whether that head is still current, and
/// the generation that tells this registration apart from a later one for the
/// same PR.
struct InFlight {
    token: CancellationToken,
    head_sha: String,
    recheck: Arc<Notify>,
    generation: u64,
}

#[derive(Default)]
struct Inner {
    map: HashMap<Key, InFlight>,
    next_generation: u64,
}

/// What [`CancelRegistry::register`] hands the job that is starting a review.
///
/// The job holds it for as long as the review runs. Dropping it unregisters
/// the review, however the job ends, unless a later registration for the same
/// PR has replaced it.
pub struct Registered {
    /// Fired when the review is to stop without posting.
    pub token: CancellationToken,
    /// Notified when a push arrived whose head differs from the one under
    /// review. The job re-reads the PR's head from GitHub and cancels `token`
    /// itself if the head it is reviewing is no longer current.
    pub recheck: Arc<Notify>,
    registry: CancelRegistry,
    key: Key,
    generation: u64,
}

impl Drop for Registered {
    fn drop(&mut self) {
        let mut inner = self.registry.lock();
        if inner
            .map
            .get(&self.key)
            .is_some_and(|e| e.generation == self.generation)
        {
            inner.map.remove(&self.key);
        }
    }
}

/// In-memory registry mapping each active PR job to its cancellation token
/// and the head SHA it is reviewing.
///
/// When a `pull_request.closed` event arrives, `cancel()` fires the token for
/// that PR. When a `pull_request.synchronize` arrives whose head differs from
/// the head under review, `push_arrived()` asks the job to re-check its head;
/// a delivery is not evidence of the PR's current head, since it may be a
/// redelivery or arrive late, so only the job, which can ask GitHub, decides
/// to cancel. Any checker task racing `select!(run | cancelled)` will then
/// abort without posting results.
#[derive(Clone, Default)]
pub struct CancelRegistry {
    inner: Arc<Mutex<Inner>>,
}

impl CancelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The lock is never held across an await or a call that can panic, so a
    /// poisoned lock still holds a consistent map.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Register this PR's in-flight job, which is reviewing `head_sha`.
    ///
    /// Returns `None` when a review of the same head is already in flight:
    /// that review covers this head, and the caller does not start a second
    /// one. A review of a different head is cancelled and replaced, since the
    /// caller has just read `head_sha` from GitHub and the other head is no
    /// longer current.
    pub async fn register(
        &self,
        owner: &str,
        repo: &str,
        pr: i64,
        head_sha: &str,
    ) -> Option<Registered> {
        let key: Key = (owner.into(), repo.into(), pr);
        let mut inner = self.lock();
        if let Some(existing) = inner.map.get(&key)
            && existing.head_sha == head_sha
            && !existing.token.is_cancelled()
        {
            return None;
        }
        let generation = inner.next_generation;
        inner.next_generation += 1;
        let token = CancellationToken::new();
        let recheck = Arc::new(Notify::new());
        let entry = InFlight {
            token: token.clone(),
            head_sha: head_sha.into(),
            recheck: recheck.clone(),
            generation,
        };
        if let Some(old) = inner.map.insert(key.clone(), entry) {
            old.token.cancel();
        }
        Some(Registered {
            token,
            recheck,
            registry: self.clone(),
            key,
            generation,
        })
    }

    /// Cancel (and remove) the token for this PR. Called when the PR is
    /// closed. Returns whether there was one.
    pub async fn cancel(&self, owner: &str, repo: &str, pr: i64) -> bool {
        let removed = self.lock().map.remove(&(owner.into(), repo.into(), pr));
        if let Some(in_flight) = removed {
            in_flight.token.cancel();
            true
        } else {
            false
        }
    }

    /// A push delivery for this PR carried `delivered_head`. If a review of a
    /// different head is in flight, ask it to re-check its head; nothing is
    /// cancelled here. Returns whether a review was asked.
    pub async fn push_arrived(
        &self,
        owner: &str,
        repo: &str,
        pr: i64,
        delivered_head: &str,
    ) -> bool {
        match self.lock().map.get(&(owner.into(), repo.into(), pr)) {
            Some(in_flight) if in_flight.head_sha != delivered_head => {
                in_flight.recheck.notify_one();
                true
            }
            _ => false,
        }
    }

    /// The head of the review registered for this PR, if one is.
    pub async fn head_in_flight(&self, owner: &str, repo: &str, pr: i64) -> Option<String> {
        self.lock()
            .map
            .get(&(owner.into(), repo.into(), pr))
            .map(|e| e.head_sha.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancel_says_whether_anything_was_in_flight() {
        let reg = CancelRegistry::new();
        assert!(!reg.cancel("o", "r", 1).await, "nothing registered yet");
        let token_reg = reg.register("o", "r", 1, "h1").await.unwrap();
        let token = token_reg.token.clone();
        assert!(reg.cancel("o", "r", 1).await, "the review in flight");
        assert!(token.is_cancelled());
        assert!(!reg.cancel("o", "r", 1).await, "already gone");
    }

    #[tokio::test]
    async fn a_push_of_another_head_asks_the_review_to_recheck_and_cancels_nothing() {
        let reg = CancelRegistry::new();
        let r = reg.register("o", "r", 1, "h1").await.unwrap();
        assert!(!reg.push_arrived("o", "r", 1, "h1").await, "same head");
        assert!(reg.push_arrived("o", "r", 1, "h2").await, "another head");
        assert!(!r.token.is_cancelled(), "the job decides, not the delivery");
        tokio::time::timeout(std::time::Duration::from_secs(1), r.recheck.notified())
            .await
            .expect("the review should have been asked to re-check");
        assert!(
            !reg.push_arrived("o", "r", 2, "h2").await,
            "nothing in flight"
        );
    }

    #[tokio::test]
    async fn a_second_review_of_the_head_in_flight_is_not_started() {
        let reg = CancelRegistry::new();
        let first = reg.register("o", "r", 1, "h1").await.unwrap();
        assert!(reg.register("o", "r", 1, "h1").await.is_none());
        assert!(!first.token.is_cancelled());
    }

    #[tokio::test]
    async fn a_review_whose_registration_was_dropped_does_not_block_the_same_head() {
        // The job ended without unregistering in any other way, such as a
        // checker panic unwinding through run_job.
        let reg = CancelRegistry::new();
        drop(reg.register("o", "r", 1, "h1").await.unwrap());
        assert!(reg.register("o", "r", 1, "h1").await.is_some());
    }

    #[tokio::test]
    async fn an_older_review_finishing_does_not_unregister_the_newer_one() {
        let reg = CancelRegistry::new();
        let older = reg.register("o", "r", 1, "h1").await.unwrap();
        let newer = reg.register("o", "r", 1, "h2").await.unwrap();
        drop(older);
        assert!(
            reg.push_arrived("o", "r", 1, "h3").await,
            "the newer review should still be registered"
        );
        assert!(!newer.token.is_cancelled());
    }

    #[tokio::test]
    async fn a_cancelled_review_of_the_same_head_does_not_block_a_new_one() {
        // A review the head watcher cancelled stays registered until its job
        // returns. If the head is force-pushed back to that commit meanwhile,
        // the new review of it must start.
        let reg = CancelRegistry::new();
        let first = reg.register("o", "r", 1, "h1").await.unwrap();
        first.token.cancel();
        assert!(reg.register("o", "r", 1, "h1").await.is_some());
    }

    #[tokio::test]
    async fn a_review_of_a_new_head_replaces_the_one_in_flight() {
        let reg = CancelRegistry::new();
        let first = reg.register("o", "r", 1, "h1").await.unwrap();
        let second = reg.register("o", "r", 1, "h2").await.unwrap();
        assert!(first.token.is_cancelled());
        assert!(!second.token.is_cancelled());
    }

    #[tokio::test]
    async fn register_returns_uncancelled_token() {
        let reg = CancelRegistry::new();
        let token_reg = reg.register("o", "r", 1, "h1").await.unwrap();
        let token = token_reg.token.clone();
        assert!(!token.is_cancelled());
    }

    #[tokio::test]
    async fn cancel_fires_registered_token() {
        let reg = CancelRegistry::new();
        let token_reg = reg.register("o", "r", 1, "h1").await.unwrap();
        let token = token_reg.token.clone();
        reg.cancel("o", "r", 1).await;
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn cancel_noop_when_no_token() {
        let reg = CancelRegistry::new();
        reg.cancel("o", "r", 99).await; // must not panic
    }

    #[tokio::test]
    async fn dropping_the_registration_unregisters_without_cancelling() {
        let reg = CancelRegistry::new();
        let r = reg.register("o", "r", 1, "h1").await.unwrap();
        let token = r.token.clone();
        drop(r);
        assert!(!token.is_cancelled());
        assert_eq!(reg.head_in_flight("o", "r", 1).await, None);
        assert!(!reg.cancel("o", "r", 1).await, "nothing left to cancel");
    }

    #[tokio::test]
    async fn tokens_are_per_pr() {
        let reg = CancelRegistry::new();
        let t1_reg = reg.register("o", "r", 1, "h1").await.unwrap();
        let t1 = t1_reg.token.clone();
        let t2_reg = reg.register("o", "r", 2, "h1").await.unwrap();
        let t2 = t2_reg.token.clone();
        reg.cancel("o", "r", 1).await;
        assert!(t1.is_cancelled());
        assert!(!t2.is_cancelled());
    }
}
