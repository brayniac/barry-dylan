use crate::storage::Store;
use crate::storage::actor::{ActorCommand, Reply};
use tokio::sync::oneshot;

#[derive(Debug, Clone)]
pub struct NewJob {
    pub installation_id: i64,
    pub repo_owner: String,
    pub repo_name: String,
    pub pr_number: i64,
    pub event_kind: String,
    pub delivery_id: String,
}

#[derive(Debug, Clone)]
pub struct LeasedJob {
    pub id: i64,
    pub installation_id: i64,
    pub repo_owner: String,
    pub repo_name: String,
    pub pr_number: i64,
    pub event_kind: String,
    pub delivery_id: String,
    pub attempts: i64,
}

impl Store {
    /// Enqueue a job, coalescing with any existing pending job for the same
    /// (repo, pr, event_kind). `now_ts` and `run_after_ts` are unix seconds.
    pub async fn enqueue(
        &self,
        job: &NewJob,
        now_ts: i64,
        run_after_ts: i64,
    ) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::Enqueue {
                job: job.clone(),
                now_ts,
                run_after: run_after_ts,
                reply: Reply { tx },
            })
            .map_err(|_| crate::storage::DbError::Closed)?;
        rx.await.map_err(|_| crate::storage::DbError::Closed)??;
        Ok(())
    }

    /// Return the run_after timestamp of the pending job, if any.
    pub async fn pending_run_after(
        &self,
        repo_owner: &str,
        repo_name: &str,
        pr_number: i64,
        event_kind: &str,
    ) -> anyhow::Result<Option<i64>> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::PendingRunAfter {
                repo_owner: repo_owner.to_string(),
                repo_name: repo_name.to_string(),
                pr: pr_number,
                event_kind: event_kind.to_string(),
                reply: Reply { tx },
            })
            .map_err(|_| crate::storage::DbError::Closed)?;
        let res: Option<i64> = rx.await.map_err(|_| crate::storage::DbError::Closed)??;
        Ok(res)
    }
}

impl Store {
    /// Lease the oldest job whose run_after <= now_ts and is not currently leased
    /// (or whose lease has expired). Returns None if there's nothing to do.
    pub async fn lease_next(
        &self,
        now_ts: i64,
        lease_secs: i64,
    ) -> anyhow::Result<Option<LeasedJob>> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::LeaseNext {
                now_ts,
                lease_secs,
                reply: Reply { tx },
            })
            .map_err(|_| crate::storage::DbError::Closed)?;
        let res: Option<LeasedJob> = rx.await.map_err(|_| crate::storage::DbError::Closed)??;
        Ok(res)
    }

    pub async fn ack(&self, job_id: i64) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::Ack {
                job_id,
                reply: Reply { tx },
            })
            .map_err(|_| crate::storage::DbError::Closed)?;
        rx.await.map_err(|_| crate::storage::DbError::Closed)??;
        Ok(())
    }

    /// Defer a leased job to a specific future time without consuming an attempt.
    /// Use this when a temporary external condition (e.g. GitHub rate limit) requires
    /// the job to wait — not when the job itself failed.
    pub async fn reschedule_at(
        &self,
        job_id: i64,
        run_after: i64,
        reason: &str,
    ) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::RescheduleAt {
                job_id,
                run_after,
                reason: reason.to_string(),
                reply: Reply { tx },
            })
            .map_err(|_| crate::storage::DbError::Closed)?;
        let res: () = rx.await.map_err(|_| crate::storage::DbError::Closed)??;
        Ok(res)
    }

    /// Delete all pending (non-leased) jobs for a PR. Used when a PR is closed.
    pub async fn cancel_pr_jobs(
        &self,
        repo_owner: &str,
        repo_name: &str,
        pr_number: i64,
    ) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::CancelPrJobs {
                repo_owner: repo_owner.to_string(),
                repo_name: repo_name.to_string(),
                pr_number,
                reply: Reply { tx },
            })
            .map_err(|_| crate::storage::DbError::Closed)?;
        rx.await.map_err(|_| crate::storage::DbError::Closed)??;
        Ok(())
    }

    /// Mark a job as failed; if attempts < max_attempts, reschedule with backoff.
    /// Returns true if the job was rescheduled, false if it was dropped.
    pub async fn nack(
        &self,
        job_id: i64,
        now_ts: i64,
        error: &str,
        max_attempts: i64,
        backoff_schedule_secs: &[i64],
    ) -> anyhow::Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::Nack {
                job_id,
                now_ts,
                error: error.to_string(),
                max_attempts,
                backoff: backoff_schedule_secs.to_vec(),
                reply: Reply { tx },
            })
            .map_err(|_| crate::storage::DbError::Closed)?;
        let res: bool = rx.await.map_err(|_| crate::storage::DbError::Closed)??;
        Ok(res)
    }
}

#[cfg(test)]
mod lease_tests {
    use super::*;

    fn job(pr: i64) -> NewJob {
        NewJob {
            installation_id: 1,
            repo_owner: "o".into(),
            repo_name: "r".into(),
            pr_number: pr,
            event_kind: "synchronize".into(),
            delivery_id: "d".into(),
        }
    }

    #[tokio::test]
    async fn lease_returns_due_job_and_marks_leased() {
        let s = Store::in_memory().await.unwrap();
        s.enqueue(&job(1), 100, 100).await.unwrap();
        let leased = s.lease_next(100, 300).await.unwrap().unwrap();
        assert_eq!(leased.pr_number, 1);
        // immediate second lease at same time should find nothing
        assert!(s.lease_next(100, 300).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn expired_lease_can_be_re_leased() {
        let s = Store::in_memory().await.unwrap();
        s.enqueue(&job(2), 100, 100).await.unwrap();
        let _ = s.lease_next(100, 60).await.unwrap().unwrap();
        // 200s later the lease has expired
        let leased = s.lease_next(200, 60).await.unwrap();
        assert!(leased.is_some());
    }

    #[tokio::test]
    async fn ack_removes_job() {
        let s = Store::in_memory().await.unwrap();
        s.enqueue(&job(3), 100, 100).await.unwrap();
        let l = s.lease_next(100, 60).await.unwrap().unwrap();
        s.ack(l.id).await.unwrap();
        let n = s.count_rows("jobs").await.unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn reschedule_at_does_not_consume_attempt() {
        let s = Store::in_memory().await.unwrap();
        s.enqueue(&job(7), 100, 100).await.unwrap();
        let l = s.lease_next(100, 60).await.unwrap().unwrap();
        s.reschedule_at(l.id, 5000, "rate limited").await.unwrap();
        // job is not visible until run_after
        assert!(s.lease_next(200, 60).await.unwrap().is_none());
        // re-leasable at run_after, attempts unchanged
        let l2 = s.lease_next(5000, 60).await.unwrap().unwrap();
        assert_eq!(l2.id, l.id);
        assert_eq!(l2.attempts, 0);
    }

    #[tokio::test]
    async fn nack_reschedules_until_max_attempts() {
        let s = Store::in_memory().await.unwrap();
        s.enqueue(&job(4), 100, 100).await.unwrap();
        let l = s.lease_next(100, 60).await.unwrap().unwrap();
        let alive = s
            .nack(l.id, 200, "boom", 3, &[60, 300, 1500])
            .await
            .unwrap();
        assert!(alive);

        let l = s.lease_next(300, 60).await.unwrap().unwrap();
        let alive = s
            .nack(l.id, 400, "boom", 3, &[60, 300, 1500])
            .await
            .unwrap();
        assert!(alive);

        let l = s.lease_next(2000, 60).await.unwrap().unwrap();
        let alive = s
            .nack(l.id, 2100, "boom", 3, &[60, 300, 1500])
            .await
            .unwrap();
        assert!(!alive);

        let n = s.count_rows("jobs").await.unwrap();
        assert_eq!(n, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(pr: i64, kind: &str, delivery: &str) -> NewJob {
        NewJob {
            installation_id: 1,
            repo_owner: "o".into(),
            repo_name: "r".into(),
            pr_number: pr,
            event_kind: kind.into(),
            delivery_id: delivery.into(),
        }
    }

    #[tokio::test]
    async fn enqueue_inserts_new_job() {
        let s = Store::in_memory().await.unwrap();
        s.enqueue(&job(1, "synchronize", "d1"), 100, 130)
            .await
            .unwrap();
        let after = s
            .pending_run_after("o", "r", 1, "synchronize")
            .await
            .unwrap();
        assert_eq!(after, Some(130));
    }

    #[tokio::test]
    async fn coalesces_pending_job() {
        let s = Store::in_memory().await.unwrap();
        s.enqueue(&job(1, "synchronize", "d1"), 100, 130)
            .await
            .unwrap();
        s.enqueue(&job(1, "synchronize", "d2"), 110, 140)
            .await
            .unwrap();
        let after = s
            .pending_run_after("o", "r", 1, "synchronize")
            .await
            .unwrap();
        assert_eq!(after, Some(140));
        let n = s.count_rows("jobs").await.unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn does_not_lower_run_after_on_coalesce() {
        let s = Store::in_memory().await.unwrap();
        s.enqueue(&job(1, "synchronize", "d1"), 100, 200)
            .await
            .unwrap();
        s.enqueue(&job(1, "synchronize", "d2"), 110, 150)
            .await
            .unwrap();
        let after = s
            .pending_run_after("o", "r", 1, "synchronize")
            .await
            .unwrap();
        assert_eq!(after, Some(200));
    }

    #[tokio::test]
    async fn different_event_kinds_are_independent() {
        let s = Store::in_memory().await.unwrap();
        s.enqueue(&job(1, "synchronize", "d1"), 100, 130)
            .await
            .unwrap();
        s.enqueue(&job(1, "opened", "d2"), 100, 130).await.unwrap();
        let n = s.count_rows("jobs").await.unwrap();
        assert_eq!(n, 2);
    }
}
