use crate::checker::multi_review::identity::Identity;
use crate::storage::Store;
use sqlx::Row;

#[derive(Debug, Clone, Copy)]
pub struct RunKey<'a> {
    pub owner: &'a str,
    pub repo: &'a str,
    pub pr: i64,
    pub head_sha: &'a str,
}

#[derive(Debug, Clone)]
pub struct RunState {
    pub barry_posted: bool,
    pub other_barry_posted: bool,
    pub other_other_barry_posted: bool,
    pub confers_used: u32,
    pub last_outcome: Option<String>,
}

impl Store {
    pub async fn record_post(
        &self,
        key: RunKey<'_>,
        identity: Identity,
        outcome: &str,
        now_ts: i64,
    ) -> anyhow::Result<()> {
        let col = match identity {
            Identity::Barry => "barry_posted",
            Identity::OtherBarry => "other_barry_posted",
            Identity::OtherOtherBarry => "other_other_barry_posted",
        };
        let sql = format!(
            "INSERT INTO multi_review_runs
              (repo_owner, repo_name, pr_number, head_sha, {col}, last_outcome, updated_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6)
             ON CONFLICT(repo_owner, repo_name, pr_number, head_sha) DO UPDATE SET
               {col} = 1, last_outcome = excluded.last_outcome, updated_at = excluded.updated_at"
        );
        sqlx::query(&sql)
            .bind(key.owner)
            .bind(key.repo)
            .bind(key.pr)
            .bind(key.head_sha)
            .bind(outcome)
            .bind(now_ts)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn record_confer_used(&self, key: RunKey<'_>, now_ts: i64) -> anyhow::Result<()> {
        sqlx::query(
            r#"INSERT INTO multi_review_runs
                (repo_owner, repo_name, pr_number, head_sha, confers_used, updated_at)
               VALUES (?1, ?2, ?3, ?4, 1, ?5)
               ON CONFLICT(repo_owner, repo_name, pr_number, head_sha) DO UPDATE SET
                 confers_used = confers_used + 1, updated_at = excluded.updated_at"#,
        )
        .bind(key.owner)
        .bind(key.repo)
        .bind(key.pr)
        .bind(key.head_sha)
        .bind(now_ts)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn run_state(&self, key: RunKey<'_>) -> anyhow::Result<Option<RunState>> {
        let row = sqlx::query(
            r#"SELECT barry_posted, other_barry_posted, other_other_barry_posted,
                      confers_used, last_outcome
               FROM multi_review_runs
               WHERE repo_owner=?1 AND repo_name=?2 AND pr_number=?3 AND head_sha=?4"#,
        )
        .bind(key.owner)
        .bind(key.repo)
        .bind(key.pr)
        .bind(key.head_sha)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| RunState {
            barry_posted: r.get::<i64, _>("barry_posted") != 0,
            other_barry_posted: r.get::<i64, _>("other_barry_posted") != 0,
            other_other_barry_posted: r.get::<i64, _>("other_other_barry_posted") != 0,
            confers_used: r.get::<i64, _>("confers_used") as u32,
            last_outcome: r.get("last_outcome"),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key<'a>() -> RunKey<'a> {
        RunKey {
            owner: "o",
            repo: "r",
            pr: 1,
            head_sha: "sha",
        }
    }

    #[tokio::test]
    async fn record_post_creates_row() {
        let s = Store::in_memory().await.unwrap();
        s.record_post(key(), Identity::Barry, "approve", 100)
            .await
            .unwrap();
        let st = s.run_state(key()).await.unwrap().unwrap();
        assert!(st.barry_posted);
        assert!(!st.other_barry_posted);
        assert_eq!(st.last_outcome.as_deref(), Some("approve"));
    }

    #[tokio::test]
    async fn record_post_updates_existing_row() {
        let s = Store::in_memory().await.unwrap();
        s.record_post(key(), Identity::Barry, "approve", 100)
            .await
            .unwrap();
        s.record_post(key(), Identity::OtherBarry, "comment", 200)
            .await
            .unwrap();
        let st = s.run_state(key()).await.unwrap().unwrap();
        assert!(st.barry_posted);
        assert!(st.other_barry_posted);
        assert_eq!(st.last_outcome.as_deref(), Some("comment"));
    }

    #[tokio::test]
    async fn no_row_for_unknown_sha() {
        let s = Store::in_memory().await.unwrap();
        let old = RunKey {
            owner: "o",
            repo: "r",
            pr: 1,
            head_sha: "sha-old",
        };
        let new = RunKey {
            owner: "o",
            repo: "r",
            pr: 1,
            head_sha: "sha-new",
        };
        s.record_post(old, Identity::Barry, "approve", 100)
            .await
            .unwrap();
        assert!(s.run_state(new).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn confer_count_increments() {
        let s = Store::in_memory().await.unwrap();
        s.record_post(key(), Identity::Barry, "approve", 100)
            .await
            .unwrap();
        s.record_confer_used(key(), 200).await.unwrap();
        s.record_confer_used(key(), 300).await.unwrap();
        let st = s.run_state(key()).await.unwrap().unwrap();
        assert_eq!(st.confers_used, 2);
    }
}
