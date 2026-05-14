use crate::checker::multi_review::identity::Identity;
use crate::storage::Store;
use crate::storage::actor::{ActorCommand, Reply};
use tokio::sync::oneshot;

/// Key for multi-review run state, owned to cross the actor boundary.
#[derive(Debug, Clone)]
pub struct RunKey {
    pub owner: String,
    pub repo: String,
    pub pr: i64,
    pub head_sha: String,
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
        key: RunKey,
        identity: Identity,
        outcome: &str,
        now_ts: i64,
    ) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::RecordPost {
                key,
                identity: identity.slug().to_string(),
                outcome: outcome.to_string(),
                now_ts,
                reply: Reply { tx },
            })
            .map_err(|_| crate::storage::DbError::Closed)?;
        rx.await.map_err(|_| crate::storage::DbError::Closed)??;
        Ok(())
    }

    pub async fn record_confer_used(&self, key: RunKey, now_ts: i64) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::RecordConferUsed {
                key,
                now_ts,
                reply: Reply { tx },
            })
            .map_err(|_| crate::storage::DbError::Closed)?;
        rx.await.map_err(|_| crate::storage::DbError::Closed)??;
        Ok(())
    }

    pub async fn run_state(&self, key: RunKey) -> anyhow::Result<Option<RunState>> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::RunState {
                key,
                reply: Reply { tx },
            })
            .map_err(|_| crate::storage::DbError::Closed)?;
        let res: Option<RunState> = rx.await.map_err(|_| crate::storage::DbError::Closed)??;
        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> RunKey {
        RunKey {
            owner: "o".to_string(),
            repo: "r".to_string(),
            pr: 1,
            head_sha: "sha".to_string(),
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
            owner: "o".to_string(),
            repo: "r".to_string(),
            pr: 1,
            head_sha: "sha-old".to_string(),
        };
        let new = RunKey {
            owner: "o".to_string(),
            repo: "r".to_string(),
            pr: 1,
            head_sha: "sha-new".to_string(),
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
