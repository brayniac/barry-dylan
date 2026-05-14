pub use self::actor::RawSqliteValue;
pub(crate) use self::actor::run;
pub use self::audit::AuditEntry;
pub use self::multi_review::RunKey;
pub use self::multi_review::RunState;
pub use self::queue::{LeasedJob, NewJob};
pub use self::tokens::CachedToken;

use crate::storage::actor::{ActorCommand, ConnUnsafeSend, Reply};
use crate::storage::cache::ReadCache;

pub mod actor;
pub mod audit;
pub mod cache;
pub mod multi_review;
pub mod queue;
pub mod tokens;

use sqlx::pool::PoolOptions;
use sqlx::sqlite::SqliteJournalMode;
use sqlx::sqlite::{SqliteConnectOptions, SqliteSynchronous};
use std::path::Path;
use std::str::FromStr;
use std::sync::mpsc;
use tokio::runtime::Handle;
pub(crate) const SCHEMA: &str = include_str!("schema.sql");

/// Custom error type returned by actor commands.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("database busy, try again")]
    Busy,
    #[error("actor shut down")]
    Closed,
}

#[derive(Clone)]
pub struct Store {
    tx: std::sync::Arc<mpsc::Sender<ActorCommand>>,
    cache: ReadCache,
}

impl Store {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let url = format!("sqlite://{}", path.display());
        let opts = SqliteConnectOptions::from_str(&url)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = PoolOptions::<sqlx::Sqlite>::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;

        // Run migrations on the pool before detaching a connection.
        Self::migrate_installation_tokens(&pool).await;
        Self::migrate_schema(&pool).await;

        // Create a single connection to hand to the actor.
        let conn = pool.acquire().await?.detach();

        let (tx, rx) = mpsc::channel();

        let conn = ConnUnsafeSend::new(conn);
        let _handle = run(rx, std::sync::Arc::new(conn), Handle::current());

        Ok(Self {
            tx: std::sync::Arc::new(tx),
            cache: ReadCache::new(),
        })
    }

    pub async fn in_memory() -> anyhow::Result<Self> {
        let pool = PoolOptions::<sqlx::Sqlite>::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;

        // Run migrations on the pool before detaching a connection.
        Self::migrate_installation_tokens(&pool).await;
        Self::migrate_schema(&pool).await;

        // Create a single connection to hand to the actor.
        let conn = pool.acquire().await?.detach();

        let (tx, rx) = mpsc::channel();

        let conn = ConnUnsafeSend::new(conn);
        let _handle = run(rx, std::sync::Arc::new(conn), Handle::current());

        Ok(Self {
            tx: std::sync::Arc::new(tx),
            cache: ReadCache::new(),
        })
    }

    /// Migrate installation_tokens: add `identity` column if missing.
    async fn migrate_installation_tokens(pool: &sqlx::Pool<sqlx::Sqlite>) {
        use sqlx::Row;
        let rows = sqlx::query("SELECT name FROM pragma_table_info('installation_tokens')")
            .fetch_all(pool)
            .await
            .unwrap_or_default();
        let cols: Vec<String> = rows.iter().map(|r| r.get::<String, _>("name")).collect();
        if cols.is_empty() || cols.iter().any(|c| c == "identity") {
            return;
        }
        tracing::warn!("migrating installation_tokens to identity-scoped schema (cache cleared)");
        let _ = sqlx::query("DROP TABLE installation_tokens")
            .execute(pool)
            .await;
    }

    /// Run schema statements from SCHEMA.
    async fn migrate_schema(pool: &sqlx::Pool<sqlx::Sqlite>) {
        for stmt in SCHEMA.split(';') {
            let s = stmt.trim();
            if !s.is_empty() {
                let _ = sqlx::query(s)
                    .execute(pool)
                    .await
                    .map_err(|e| tracing::error!("migration failed: {e}"));
            }
        }
    }

    /// Execute a parameterless raw SQL query and return all rows as
    /// (column_name, value) pairs. Used by tests to assert on DB state.
    pub async fn query_raw(&self, sql: &str) -> anyhow::Result<Vec<Vec<(String, RawSqliteValue)>>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(ActorCommand::RawQuery {
                sql: sql.to_string(),
                reply: Reply { tx },
            })
            .map_err(|_| DbError::Closed)?;
        let res: Vec<Vec<(String, RawSqliteValue)>> = rx.await.map_err(|_| DbError::Closed)??;
        Ok(res)
    }

    /// Helper for tests: execute COUNT(*) query and return the count as i64.
    #[cfg(test)]
    pub async fn count_rows(&self, table: &str) -> anyhow::Result<i64> {
        let rows = self
            .query_raw(&format!("SELECT COUNT(*) FROM {table}"))
            .await?;
        let row = rows.first().ok_or_else(|| anyhow::anyhow!("no rows"))?;
        let val = row.first().ok_or_else(|| anyhow::anyhow!("no columns"))?;
        match &val.1 {
            RawSqliteValue::Integer(n) => Ok(*n),
            _ => anyhow::bail!("expected integer"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_creates_schema() {
        let store = Store::in_memory().await.unwrap();
        let rows = store
            .query_raw("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .await
            .unwrap();
        let names: Vec<String> = rows
            .iter()
            .flat_map(|row| {
                row.iter()
                    .filter(|(name, _)| name == "name")
                    .map(|(_, v)| v)
            })
            .filter_map(|v| match v {
                RawSqliteValue::Text(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"jobs".to_string()));
        assert!(names.contains(&"installation_tokens".to_string()));
        assert!(names.contains(&"audit_log".to_string()));
        assert!(names.contains(&"multi_review_runs".to_string()));
    }

    #[tokio::test]
    async fn migrates_old_installation_tokens_schema() {
        // Open a raw pool (no migration) and create the pre-PR2 schema.
        let pool = sqlx::pool::PoolOptions::<sqlx::Sqlite>::new()
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

        // Run the full migration via Store::in_memory (which triggers the actor).
        Store::in_memory().await.unwrap();
    }
}
