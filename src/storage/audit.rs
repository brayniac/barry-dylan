use crate::storage::Store;
use crate::storage::actor::{ActorCommand, Reply};
use tokio::sync::oneshot;

#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub ts: i64,
    pub delivery_id: Option<String>,
    pub repo_owner: Option<String>,
    pub repo_name: Option<String>,
    pub pr_number: Option<i64>,
    pub checker_name: Option<String>,
    pub outcome: String,
    pub duration_ms: Option<i64>,
    pub details: Option<String>,
}

impl Store {
    pub async fn append_audit(&self, e: &AuditEntry) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::AppendAudit {
                entry: AuditEntry {
                    ts: e.ts,
                    delivery_id: e.delivery_id.clone(),
                    repo_owner: e.repo_owner.clone(),
                    repo_name: e.repo_name.clone(),
                    pr_number: e.pr_number,
                    checker_name: e.checker_name.clone(),
                    outcome: e.outcome.clone(),
                    duration_ms: e.duration_ms,
                    details: e.details.clone(),
                },
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
    async fn append_and_count() {
        let s = Store::in_memory().await.unwrap();
        s.append_audit(&AuditEntry {
            ts: 1,
            delivery_id: Some("d1".to_string()),
            repo_owner: Some("o".to_string()),
            repo_name: Some("r".to_string()),
            pr_number: Some(1),
            checker_name: Some("hygiene.title".to_string()),
            outcome: "success".to_string(),
            duration_ms: Some(10),
            details: None,
        })
        .await
        .unwrap();
        let n = s.count_rows("audit_log").await.unwrap();
        assert_eq!(n, 1);
    }
}
