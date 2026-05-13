use crate::storage::Store;

#[derive(Debug, Clone)]
pub struct AuditEntry<'a> {
    pub ts: i64,
    pub delivery_id: Option<&'a str>,
    pub repo_owner: Option<&'a str>,
    pub repo_name: Option<&'a str>,
    pub pr_number: Option<i64>,
    pub checker_name: Option<&'a str>,
    pub outcome: &'a str,
    pub duration_ms: Option<i64>,
    pub details: Option<&'a str>,
}

impl Store {
    pub async fn append_audit(&self, e: &AuditEntry<'_>) -> anyhow::Result<()> {
        sqlx::query(
            r#"INSERT INTO audit_log
               (ts, delivery_id, repo_owner, repo_name, pr_number, checker_name, outcome, duration_ms, details)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"#,
        )
        .bind(e.ts)
        .bind(e.delivery_id)
        .bind(e.repo_owner)
        .bind(e.repo_name)
        .bind(e.pr_number)
        .bind(e.checker_name)
        .bind(e.outcome)
        .bind(e.duration_ms)
        .bind(e.details)
        .execute(&self.pool).await?;
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
            delivery_id: Some("d1"),
            repo_owner: Some("o"),
            repo_name: Some("r"),
            pr_number: Some(1),
            checker_name: Some("hygiene.title"),
            outcome: "success",
            duration_ms: Some(10),
            details: None,
        })
        .await
        .unwrap();
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
    }
}
