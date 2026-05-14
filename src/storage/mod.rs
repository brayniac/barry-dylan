use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::path::Path;
use std::str::FromStr;

pub mod audit;
pub mod multi_review;
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
        migrate_installation_tokens(pool).await?;
        for stmt in SCHEMA.split(';') {
            let s = stmt.trim();
            if !s.is_empty() {
                sqlx::query(s).execute(pool).await?;
            }
        }
        Ok(())
    }
}

async fn migrate_installation_tokens(pool: &SqlitePool) -> anyhow::Result<()> {
    let cols: Vec<(String,)> =
        sqlx::query_as("SELECT name FROM pragma_table_info('installation_tokens')")
            .fetch_all(pool)
            .await?;
    let cols: Vec<String> = cols.into_iter().map(|t| t.0).collect();
    if cols.is_empty() {
        return Ok(()); // table not yet created
    }
    if cols.iter().any(|c| c == "identity") {
        return Ok(()); // already migrated
    }
    tracing::warn!("migrating installation_tokens to identity-scoped schema (cache cleared)");
    sqlx::query("DROP TABLE installation_tokens")
        .execute(pool)
        .await?;
    Ok(())
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
        assert!(names.contains(&"multi_review_runs".to_string()));
    }

    #[tokio::test]
    async fn migrates_old_installation_tokens_schema() {
        // Open a raw pool (no migration) and create the pre-PR2 schema.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE installation_tokens \
             (installation_id INTEGER PRIMARY KEY, token TEXT NOT NULL, expires_at INTEGER NOT NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO installation_tokens (installation_id, token, expires_at) VALUES (1, 'tok', 9999999999)",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Run the full migration.
        Store::migrate(&pool).await.unwrap();

        // The table should now have an `identity` column.
        let cols: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM pragma_table_info('installation_tokens')")
                .fetch_all(&pool)
                .await
                .unwrap();
        let col_names: Vec<String> = cols.into_iter().map(|t| t.0).collect();
        assert!(
            col_names.iter().any(|c| c == "identity"),
            "expected identity column after migration, got: {col_names:?}"
        );
    }
}
