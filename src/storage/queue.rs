use crate::storage::Store;
use serde::{Deserialize, Serialize};
use sqlx::Row;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
        let mut tx = self.pool.begin().await?;

        // Try update first (coalesce); if no row affected, insert.
        let updated = sqlx::query(
            r#"UPDATE jobs
               SET run_after = MAX(run_after, ?1),
                   delivery_id = ?2
               WHERE repo_owner = ?3 AND repo_name = ?4 AND pr_number = ?5
                 AND event_kind = ?6 AND leased_until IS NULL"#,
        )
        .bind(run_after_ts)
        .bind(&job.delivery_id)
        .bind(&job.repo_owner)
        .bind(&job.repo_name)
        .bind(job.pr_number)
        .bind(&job.event_kind)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        if updated == 0 {
            sqlx::query(
                r#"INSERT INTO jobs
                    (installation_id, repo_owner, repo_name, pr_number, event_kind,
                     delivery_id, received_at, run_after)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"#,
            )
            .bind(job.installation_id)
            .bind(&job.repo_owner)
            .bind(&job.repo_name)
            .bind(job.pr_number)
            .bind(&job.event_kind)
            .bind(&job.delivery_id)
            .bind(now_ts)
            .bind(run_after_ts)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
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
        let row = sqlx::query(
            r#"SELECT run_after FROM jobs
               WHERE repo_owner = ?1 AND repo_name = ?2 AND pr_number = ?3
                 AND event_kind = ?4 AND leased_until IS NULL"#,
        )
        .bind(repo_owner)
        .bind(repo_name)
        .bind(pr_number)
        .bind(event_kind)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.get::<i64, _>("run_after")))
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
        // Single atomic UPDATE ... RETURNING. SQLite serializes writers, so
        // concurrent worker polls cannot deadlock on lock escalation the way
        // a SELECT-then-UPDATE pair under BEGIN DEFERRED can.
        let row = sqlx::query(
            r#"UPDATE jobs
                  SET leased_until = ?1
                WHERE id = (
                  SELECT id FROM jobs
                  WHERE run_after <= ?2
                    AND (leased_until IS NULL OR leased_until <= ?2)
                  ORDER BY run_after ASC, id ASC
                  LIMIT 1
                )
                RETURNING id, installation_id, repo_owner, repo_name, pr_number,
                          event_kind, delivery_id, attempts"#,
        )
        .bind(now_ts + lease_secs)
        .bind(now_ts)
        .fetch_optional(&self.pool)
        .await?;
        let Some(r) = row else { return Ok(None) };
        Ok(Some(LeasedJob {
            id: r.get("id"),
            installation_id: r.get("installation_id"),
            repo_owner: r.get("repo_owner"),
            repo_name: r.get("repo_name"),
            pr_number: r.get("pr_number"),
            event_kind: r.get("event_kind"),
            delivery_id: r.get("delivery_id"),
            attempts: r.get("attempts"),
        }))
    }

    pub async fn ack(&self, job_id: i64) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM jobs WHERE id = ?1")
            .bind(job_id)
            .execute(&self.pool)
            .await?;
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
        sqlx::query(
            r#"UPDATE jobs
               SET leased_until = NULL, run_after = ?1, last_error = ?2
               WHERE id = ?3"#,
        )
        .bind(run_after)
        .bind(reason)
        .bind(job_id)
        .execute(&self.pool)
        .await?;
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
        let row = sqlx::query("SELECT attempts FROM jobs WHERE id = ?1")
            .bind(job_id)
            .fetch_optional(&self.pool)
            .await?;
        let Some(r) = row else { return Ok(false) };
        let attempts: i64 = r.get("attempts");
        let next_attempts = attempts + 1;

        if next_attempts >= max_attempts {
            sqlx::query("DELETE FROM jobs WHERE id = ?1")
                .bind(job_id)
                .execute(&self.pool)
                .await?;
            return Ok(false);
        }
        let idx = (attempts as usize).min(backoff_schedule_secs.len().saturating_sub(1));
        let delay = backoff_schedule_secs.get(idx).copied().unwrap_or(60);
        sqlx::query(
            r#"UPDATE jobs
               SET attempts = ?1, leased_until = NULL, run_after = ?2, last_error = ?3
               WHERE id = ?4"#,
        )
        .bind(next_attempts)
        .bind(now_ts + delay)
        .bind(error)
        .bind(job_id)
        .execute(&self.pool)
        .await?;
        Ok(true)
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
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM jobs")
            .fetch_one(&s.pool)
            .await
            .unwrap();
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

        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM jobs")
            .fetch_one(&s.pool)
            .await
            .unwrap();
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
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM jobs")
            .fetch_one(&s.pool)
            .await
            .unwrap();
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
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM jobs")
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(n, 2);
    }
}
