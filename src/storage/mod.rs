use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::path::Path;
use std::str::FromStr;

pub mod audit;
pub mod queue;
pub mod tokens;

const SCHEMA: &str = include_str!("schema.sql");

#[derive(Clone)]
pub struct Store {
    pub pool: SqlitePool,
}

impl Store {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let url = format!("sqlite://{}", path.display());
        let opts = SqliteConnectOptions::from_str(&url)?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;
        Self::migrate(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn in_memory() -> anyhow::Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        Self::migrate(&pool).await?;
        Ok(Self { pool })
    }

    async fn migrate(pool: &SqlitePool) -> anyhow::Result<()> {
        for stmt in SCHEMA.split(';') {
            let s = stmt.trim();
            if !s.is_empty() {
                sqlx::query(s).execute(pool).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_creates_schema() {
        let store = Store::in_memory().await.unwrap();
        let names: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        let names: Vec<String> = names.into_iter().map(|t| t.0).collect();
        assert!(names.contains(&"jobs".to_string()));
        assert!(names.contains(&"installation_tokens".to_string()));
        assert!(names.contains(&"audit_log".to_string()));
    }
}
