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
        .bind(repo_owner).bind(repo_name).bind(pr_number).bind(event_kind)
        .fetch_optional(&self.pool).await?;
        Ok(row.map(|r| r.get::<i64, _>("run_after")))
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
        s.enqueue(&job(1, "synchronize", "d1"), 100, 130).await.unwrap();
        let after = s.pending_run_after("o", "r", 1, "synchronize").await.unwrap();
        assert_eq!(after, Some(130));
    }

    #[tokio::test]
    async fn coalesces_pending_job() {
        let s = Store::in_memory().await.unwrap();
        s.enqueue(&job(1, "synchronize", "d1"), 100, 130).await.unwrap();
        s.enqueue(&job(1, "synchronize", "d2"), 110, 140).await.unwrap();
        let after = s.pending_run_after("o", "r", 1, "synchronize").await.unwrap();
        assert_eq!(after, Some(140));
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM jobs")
            .fetch_one(&s.pool).await.unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn does_not_lower_run_after_on_coalesce() {
        let s = Store::in_memory().await.unwrap();
        s.enqueue(&job(1, "synchronize", "d1"), 100, 200).await.unwrap();
        s.enqueue(&job(1, "synchronize", "d2"), 110, 150).await.unwrap();
        let after = s.pending_run_after("o", "r", 1, "synchronize").await.unwrap();
        assert_eq!(after, Some(200));
    }

    #[tokio::test]
    async fn different_event_kinds_are_independent() {
        let s = Store::in_memory().await.unwrap();
        s.enqueue(&job(1, "synchronize", "d1"), 100, 130).await.unwrap();
        s.enqueue(&job(1, "opened", "d2"), 100, 130).await.unwrap();
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM jobs")
            .fetch_one(&s.pool).await.unwrap();
        assert_eq!(n, 2);
    }
}
