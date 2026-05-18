use crate::storage::DbError;
use crate::storage::audit::AuditEntry;
use crate::storage::multi_review::{RunKey, RunState};
use crate::storage::queue::{LeasedJob, NewJob};
use crate::storage::installation_cache::CachedInstallation;
use crate::storage::tokens::CachedToken;
use sqlx::{Column, Connection, Row};
use std::cell::UnsafeCell;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tokio::runtime::Handle;

/// Wrapper around `UnsafeCell<SqliteConnection>` that is safe to send across threads.
/// The actor pattern guarantees single-threaded access, so this is safe.
pub(crate) struct ConnUnsafeSend(UnsafeCell<sqlx::SqliteConnection>);

unsafe impl Send for ConnUnsafeSend {}
unsafe impl Sync for ConnUnsafeSend {}

impl ConnUnsafeSend {
    pub(crate) fn new(conn: sqlx::SqliteConnection) -> Self {
        Self(UnsafeCell::new(conn))
    }
}

/// A single SQLite value returned from a raw query.
#[derive(Clone, Debug)]
pub enum RawSqliteValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// One-shot reply wrapper for actor commands.
pub struct Reply<T> {
    pub tx: tokio::sync::oneshot::Sender<Result<T, DbError>>,
}

impl<T> Reply<T> {
    pub fn send(self, result: Result<T, DbError>) {
        let _ = self.tx.send(result);
    }
}

/// All commands the actor can process.
pub enum ActorCommand {
    LeaseNext {
        now_ts: i64,
        lease_secs: i64,
        reply: Reply<Option<LeasedJob>>,
    },
    Ack {
        job_id: i64,
        reply: Reply<()>,
    },
    RescheduleAt {
        job_id: i64,
        run_after: i64,
        reason: String,
        reply: Reply<()>,
    },
    Nack {
        job_id: i64,
        now_ts: i64,
        error: String,
        max_attempts: i64,
        backoff: Vec<i64>,
        reply: Reply<bool>,
    },
    Enqueue {
        job: NewJob,
        now_ts: i64,
        run_after: i64,
        reply: Reply<()>,
    },
    PendingRunAfter {
        repo_owner: String,
        repo_name: String,
        pr: i64,
        event_kind: String,
        reply: Reply<Option<i64>>,
    },
    GetTokenFor {
        identity: String,
        installation_id: i64,
        now_ts: i64,
        reply: Reply<Option<CachedToken>>,
    },
    PutTokenFor {
        identity: String,
        installation_id: i64,
        token: String,
        expires_at: i64,
        reply: Reply<()>,
    },
    GetInstallation {
        identity: String,
        owner: String,
        now_ts: i64,
        reply: Reply<Option<CachedInstallation>>,
    },
    PutInstallation {
        identity: String,
        owner: String,
        installation_id: Option<i64>,
        cached_at: i64,
        reply: Reply<()>,
    },
    InvalidateInstallation {
        identity: String,
        owner: String,
        reply: Reply<()>,
    },
    GetToken {
        installation_id: i64,
        now_ts: i64,
        reply: Reply<Option<CachedToken>>,
    },
    PutToken {
        installation_id: i64,
        token: String,
        expires_at: i64,
        reply: Reply<()>,
    },
    RecordPost {
        key: RunKey,
        identity: String,
        outcome: String,
        now_ts: i64,
        reply: Reply<()>,
    },
    RecordConferUsed {
        key: RunKey,
        now_ts: i64,
        reply: Reply<()>,
    },
    RunState {
        key: RunKey,
        reply: Reply<Option<RunState>>,
    },
    AppendAudit {
        entry: AuditEntry,
        reply: Reply<()>,
    },
    RawQuery {
        sql: String,
        reply: Reply<Vec<Vec<(String, RawSqliteValue)>>>,
    },
    CancelPrJobs {
        repo_owner: String,
        repo_name: String,
        pr_number: i64,
        reply: Reply<()>,
    },
}

/// RAII guard that logs when the actor thread exits. A normal shutdown drops
/// it during stack unwind from the sender being dropped; a panic also runs the
/// `Drop`, so either way the exit is visible in logs.
struct ActorExitLogger;

impl Drop for ActorExitLogger {
    fn drop(&mut self) {
        if std::thread::panicking() {
            tracing::error!("storage actor thread panicked; subsequent DB calls will fail");
        } else {
            tracing::info!("storage actor thread exiting");
        }
    }
}

/// Maximum number of retries on SQLITE_BUSY.
const BUSY_RETRY_MAX: u32 = 3;
/// Base delay for exponential backoff on SQLITE_BUSY (milliseconds).
const BUSY_RETRY_BASE_MS: u64 = 100;

/// Check if a sqlx::Error is a SQLITE_BUSY error.
fn is_busy_error(err: &sqlx::Error) -> bool {
    if let sqlx::Error::Database(d) = err {
        d.code() == Some(std::borrow::Cow::Borrowed("SQLITE_BUSY"))
    } else {
        false
    }
}

/// Record timing for a database operation.
fn record_db_timing(operation: &'static str, duration_ms: u64) {
    metrics::histogram!("barry_db_duration_ms", "operation" => operation)
        .record(duration_ms as f64);
}

/// Retry a closure with exponential backoff on SQLITE_BUSY.
///
/// The actor runs on a dedicated OS thread (not a Tokio worker), so calling
/// `Handle::block_on` here is safe — it does not stall the runtime. The handle
/// passed in MUST be the multi-threaded runtime's handle: blocking the current
/// thread inside a worker thread would deadlock. Callers go through
/// `storage::Store::open` / `Store::in_memory`, which capture `Handle::current()`
/// from a multi-threaded runtime.
fn retry_busy<F, Fut, T>(handle: &Handle, mut f: F) -> Result<T, DbError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, sqlx::Error>>,
{
    let mut retries: u32 = 0;
    loop {
        match handle.block_on(f()) {
            Ok(v) => return Ok(v),
            Err(e) if is_busy_error(&e) && retries < BUSY_RETRY_MAX => {
                let delay_ms = BUSY_RETRY_BASE_MS * (1u64 << retries);
                thread::sleep(Duration::from_millis(delay_ms));
                retries += 1;
            }
            Err(e) => {
                if is_busy_error(&e) {
                    return Err(DbError::Busy);
                }
                return Err(DbError::Database(e));
            }
        }
    }
}

/// The actor task that owns the SQLite connection and processes commands.
pub(crate) fn run(
    rx: mpsc::Receiver<ActorCommand>,
    conn: std::sync::Arc<ConnUnsafeSend>,
    rt: Handle,
) -> thread::JoinHandle<()> {
    std::thread::spawn(move || {
        // The actor owns the only handle to the SQLite connection; if this loop
        // exits (channel closed or panic), every future Store::* call returns
        // DbError::Closed and the app is effectively dead. We log on exit so the
        // failure is visible rather than silent.
        let _guard = ActorExitLogger;
        while let Ok(msg) = rx.recv() {
            let raw = conn.0.get();
            match msg {
                ActorCommand::LeaseNext {
                    now_ts,
                    lease_secs,
                    reply,
                } => {
                    let start = std::time::Instant::now();
                    let result = retry_busy(&rt, || async move {
                        sqlx::query(
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
                                          event_kind, delivery_id, attempts, actor"#,
                        )
                        .bind(now_ts + lease_secs)
                        .bind(now_ts)
                        .fetch_optional(unsafe { &mut *raw })
                        .await
                    });
                    let job = result.ok().flatten().map(|row| LeasedJob {
                        id: row.get("id"),
                        installation_id: row.get("installation_id"),
                        repo_owner: row.get("repo_owner"),
                        repo_name: row.get("repo_name"),
                        pr_number: row.get("pr_number"),
                        event_kind: row.get("event_kind"),
                        delivery_id: row.get("delivery_id"),
                        attempts: row.get("attempts"),
                        actor: row.get("actor"),
                    });
                    let duration_ms = start.elapsed().as_millis() as u64;
                    metrics::histogram!("barry_db_duration_ms", "operation" => "lease_next")
                        .record(duration_ms as f64);
                    reply.send(Ok(job))
                }
                ActorCommand::Ack { job_id, reply } => {
                    let start = std::time::Instant::now();
                    let result = retry_busy(&rt, || async move {
                        sqlx::query("DELETE FROM jobs WHERE id = ?1")
                            .bind(job_id)
                            .execute(unsafe { &mut *raw })
                            .await
                            .map(|_| ())
                    });
                    let duration_ms = start.elapsed().as_millis() as u64;
                    record_db_timing("ack", duration_ms);
                    reply.send(result)
                }
                ActorCommand::RescheduleAt {
                    job_id,
                    run_after,
                    reason,
                    reply,
                } => {
                    let start = std::time::Instant::now();
                    let result = retry_busy(&rt, || {
                        let reason = reason.clone();
                        async move {
                            sqlx::query(
                                r#"UPDATE jobs
                                   SET leased_until = NULL, run_after = ?1, last_error = ?2
                                   WHERE id = ?3"#,
                            )
                            .bind(run_after)
                            .bind(&reason)
                            .bind(job_id)
                            .execute(unsafe { &mut *raw })
                            .await
                            .map(|_| ())
                        }
                    });
                    let duration_ms = start.elapsed().as_millis() as u64;
                    record_db_timing("reschedule", duration_ms);
                    reply.send(result)
                }
                ActorCommand::Nack {
                    job_id,
                    now_ts,
                    error,
                    max_attempts,
                    backoff,
                    reply,
                } => {
                    let start = std::time::Instant::now();
                    let result = retry_busy(&rt, || {
                        let error = error.clone();
                        let backoff = backoff.clone();
                        async move {
                            let row = sqlx::query("SELECT attempts FROM jobs WHERE id = ?1")
                                .bind(job_id)
                                .fetch_optional(unsafe { &mut *raw })
                                .await?;
                            let Some(r) = row else { return Ok(false) };
                            let attempts: i64 = r.get("attempts");
                            let next_attempts = attempts + 1;

                            if next_attempts >= max_attempts {
                                sqlx::query("DELETE FROM jobs WHERE id = ?1")
                                    .bind(job_id)
                                    .execute(unsafe { &mut *raw })
                                    .await?;
                                return Ok(false);
                            }
                            let idx = (attempts as usize).min(backoff.len().saturating_sub(1));
                            let delay = backoff.get(idx).copied().unwrap_or(60);
                            sqlx::query(
                                r#"UPDATE jobs
                                   SET attempts = ?1, leased_until = NULL, run_after = ?2, last_error = ?3
                                   WHERE id = ?4"#,
                            )
                            .bind(next_attempts)
                            .bind(now_ts + delay)
                            .bind(&error)
                            .bind(job_id)
                            .execute(unsafe { &mut *raw })
                            .await?;
                            Ok(true)
                        }
                    });
                    let duration_ms = start.elapsed().as_millis() as u64;
                    record_db_timing("nack", duration_ms);
                    reply.send(result)
                }
                ActorCommand::Enqueue {
                    job,
                    now_ts,
                    run_after,
                    reply,
                } => {
                    let start = std::time::Instant::now();
                    let result = retry_busy(&rt, || {
                        let job = job.clone();
                        async move {
                            let mut tx = unsafe { &mut *raw }.begin().await?;

                            let updated = sqlx::query(
                                r#"UPDATE jobs
                                   SET run_after = MAX(run_after, ?1),
                                       delivery_id = ?2,
                                       actor = ?7
                                   WHERE repo_owner = ?3 AND repo_name = ?4 AND pr_number = ?5
                                     AND event_kind = ?6 AND leased_until IS NULL"#,
                            )
                            .bind(run_after)
                            .bind(&job.delivery_id)
                            .bind(&job.repo_owner)
                            .bind(&job.repo_name)
                            .bind(job.pr_number)
                            .bind(&job.event_kind)
                            .bind(&job.actor)
                            .execute(&mut *tx)
                            .await?
                            .rows_affected();

                            if updated == 0 {
                                sqlx::query(
                                    r#"INSERT INTO jobs
                                        (installation_id, repo_owner, repo_name, pr_number, event_kind,
                                         delivery_id, received_at, run_after, actor)
                                       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"#,
                                )
                                .bind(job.installation_id)
                                .bind(&job.repo_owner)
                                .bind(&job.repo_name)
                                .bind(job.pr_number)
                                .bind(&job.event_kind)
                                .bind(&job.delivery_id)
                                .bind(now_ts)
                                .bind(run_after)
                                .bind(&job.actor)
                                .execute(&mut *tx)
                                .await?;
                            }

                            tx.commit().await?;
                            Ok(())
                        }
                    });
                    let duration_ms = start.elapsed().as_millis() as u64;
                    record_db_timing("enqueue", duration_ms);
                    reply.send(result)
                }
                ActorCommand::PendingRunAfter {
                    repo_owner,
                    repo_name,
                    pr,
                    event_kind,
                    reply,
                } => {
                    let start = std::time::Instant::now();
                    let result = retry_busy(&rt, || {
                        let repo_owner = repo_owner.clone();
                        let repo_name = repo_name.clone();
                        let event_kind = event_kind.clone();
                        async move {
                            let row = sqlx::query(
                                r#"SELECT run_after FROM jobs
                                   WHERE repo_owner = ?1 AND repo_name = ?2 AND pr_number = ?3
                                     AND event_kind = ?4 AND leased_until IS NULL"#,
                            )
                            .bind(&repo_owner)
                            .bind(&repo_name)
                            .bind(pr)
                            .bind(&event_kind)
                            .fetch_optional(unsafe { &mut *raw })
                            .await?;
                            Ok(row.map(|r| r.get::<i64, _>("run_after")))
                        }
                    });
                    let duration_ms = start.elapsed().as_millis() as u64;
                    record_db_timing("pending_run_after", duration_ms);
                    reply.send(result)
                }
                ActorCommand::GetTokenFor {
                    identity,
                    installation_id,
                    now_ts,
                    reply,
                } => {
                    let start = std::time::Instant::now();
                    let result = retry_busy(&rt, || {
                        let identity = identity.clone();
                        async move {
                            let row = sqlx::query(
                                "SELECT token, expires_at FROM installation_tokens \
                                 WHERE installation_id = ?1 AND identity = ?2",
                            )
                            .bind(installation_id)
                            .bind(&identity)
                            .fetch_optional(unsafe { &mut *raw })
                            .await?;
                            Ok(row.and_then(|r| {
                                let token: String = r.get("token");
                                let expires_at: i64 = r.get("expires_at");
                                if expires_at - 60 > now_ts {
                                    Some(CachedToken { token, expires_at })
                                } else {
                                    None
                                }
                            }))
                        }
                    });
                    let duration_ms = start.elapsed().as_millis() as u64;
                    record_db_timing("get_token_for", duration_ms);
                    reply.send(result)
                }
                ActorCommand::PutTokenFor {
                    identity,
                    installation_id,
                    token,
                    expires_at,
                    reply,
                } => {
                    let start = std::time::Instant::now();
                    let result = retry_busy(&rt, || {
                        let identity = identity.clone();
                        let token = token.clone();
                        async move {
                            sqlx::query(
                                r#"INSERT INTO installation_tokens (installation_id, identity, token, expires_at)
                                   VALUES (?1, ?2, ?3, ?4)
                                   ON CONFLICT(installation_id, identity) DO UPDATE SET token=excluded.token, expires_at=excluded.expires_at"#,
                            )
                            .bind(installation_id)
                            .bind(&identity)
                            .bind(&token)
                            .bind(expires_at)
                            .execute(unsafe { &mut *raw })
                            .await
                            .map(|_| ())
                        }
                    });
                    let duration_ms = start.elapsed().as_millis() as u64;
                    record_db_timing("put_token_for", duration_ms);
                    reply.send(result)
                }
                ActorCommand::GetInstallation {
                    identity,
                    owner,
                    now_ts,
                    reply,
                } => {
                    let start = std::time::Instant::now();
                    let result = retry_busy(&rt, || {
                        let identity = identity.clone();
                        let owner = owner.clone();
                        async move {
                            let row = sqlx::query(
                                "SELECT installation_id, cached_at FROM installation_cache \
                                 WHERE identity = ?1 AND owner = ?2",
                            )
                            .bind(&identity)
                            .bind(&owner)
                            .fetch_optional(unsafe { &mut *raw })
                            .await?;
                            Ok(row.and_then(|r| {
                                let id: Option<i64> = r.get("installation_id");
                                let cached_at: i64 = r.get("cached_at");
                                match id {
                                    Some(installation_id) => Some(CachedInstallation::Cached {
                                        installation_id,
                                    }),
                                    // Negative cache: 1h TTL.
                                    None if now_ts - cached_at <= 3600 => {
                                        Some(CachedInstallation::NotInstalled)
                                    }
                                    None => None, // stale negative → treat as miss
                                }
                            }))
                        }
                    });
                    let duration_ms = start.elapsed().as_millis() as u64;
                    record_db_timing("get_installation", duration_ms);
                    reply.send(result)
                }
                ActorCommand::PutInstallation {
                    identity,
                    owner,
                    installation_id,
                    cached_at,
                    reply,
                } => {
                    let start = std::time::Instant::now();
                    let result = retry_busy(&rt, || {
                        let identity = identity.clone();
                        let owner = owner.clone();
                        async move {
                            sqlx::query(
                                r#"INSERT INTO installation_cache (identity, owner, installation_id, cached_at)
                                   VALUES (?1, ?2, ?3, ?4)
                                   ON CONFLICT(identity, owner) DO UPDATE SET
                                     installation_id = excluded.installation_id,
                                     cached_at = excluded.cached_at"#,
                            )
                            .bind(&identity)
                            .bind(&owner)
                            .bind(installation_id)
                            .bind(cached_at)
                            .execute(unsafe { &mut *raw })
                            .await
                            .map(|_| ())
                        }
                    });
                    let duration_ms = start.elapsed().as_millis() as u64;
                    record_db_timing("put_installation", duration_ms);
                    reply.send(result)
                }
                ActorCommand::InvalidateInstallation {
                    identity,
                    owner,
                    reply,
                } => {
                    let start = std::time::Instant::now();
                    let result = retry_busy(&rt, || {
                        let identity = identity.clone();
                        let owner = owner.clone();
                        async move {
                            sqlx::query(
                                "DELETE FROM installation_cache WHERE identity = ?1 AND owner = ?2",
                            )
                            .bind(&identity)
                            .bind(&owner)
                            .execute(unsafe { &mut *raw })
                            .await
                            .map(|_| ())
                        }
                    });
                    let duration_ms = start.elapsed().as_millis() as u64;
                    record_db_timing("invalidate_installation", duration_ms);
                    reply.send(result)
                }
                ActorCommand::GetToken {
                    installation_id,
                    now_ts,
                    reply,
                } => {
                    let start = std::time::Instant::now();
                    let result = retry_busy(&rt, || async move {
                        let row = sqlx::query(
                            "SELECT token, expires_at FROM installation_tokens \
                             WHERE installation_id = ?1 AND identity = ?2",
                        )
                        .bind(installation_id)
                        .bind("barry")
                        .fetch_optional(unsafe { &mut *raw })
                        .await?;
                        Ok(row.and_then(|r| {
                            let token: String = r.get("token");
                            let expires_at: i64 = r.get("expires_at");
                            if expires_at - 60 > now_ts {
                                Some(CachedToken { token, expires_at })
                            } else {
                                None
                            }
                        }))
                    });
                    let duration_ms = start.elapsed().as_millis() as u64;
                    record_db_timing("get_token", duration_ms);
                    reply.send(result)
                }
                ActorCommand::PutToken {
                    installation_id,
                    token,
                    expires_at,
                    reply,
                } => {
                    let start = std::time::Instant::now();
                    let result = retry_busy(&rt, || {
                        let token = token.clone();
                        async move {
                            sqlx::query(
                                r#"INSERT INTO installation_tokens (installation_id, identity, token, expires_at)
                                   VALUES (?1, ?2, ?3, ?4)
                                   ON CONFLICT(installation_id, identity) DO UPDATE SET token=excluded.token, expires_at=excluded.expires_at"#,
                            )
                            .bind(installation_id)
                            .bind("barry")
                            .bind(&token)
                            .bind(expires_at)
                            .execute(unsafe { &mut *raw })
                            .await
                            .map(|_| ())
                        }
                    });
                    let duration_ms = start.elapsed().as_millis() as u64;
                    record_db_timing("put_token", duration_ms);
                    reply.send(result)
                }
                ActorCommand::RecordPost {
                    key,
                    identity,
                    outcome,
                    now_ts,
                    reply,
                } => {
                    let start = std::time::Instant::now();
                    // Map identity to a fixed SQL string literal. Returning a typed
                    // error on unknown values keeps a single bad input from killing
                    // the actor thread and avoids any column-name interpolation
                    // ambiguity around the bound `?` parameters.
                    let sql: Option<&'static str> = match identity.as_str() {
                        "barry" => Some(
                            "INSERT INTO multi_review_runs
                              (repo_owner, repo_name, pr_number, head_sha, barry_posted, last_outcome, updated_at)
                             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6)
                             ON CONFLICT(repo_owner, repo_name, pr_number, head_sha) DO UPDATE SET
                               barry_posted = 1, last_outcome = excluded.last_outcome, updated_at = excluded.updated_at",
                        ),
                        "other_barry" => Some(
                            "INSERT INTO multi_review_runs
                              (repo_owner, repo_name, pr_number, head_sha, other_barry_posted, last_outcome, updated_at)
                             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6)
                             ON CONFLICT(repo_owner, repo_name, pr_number, head_sha) DO UPDATE SET
                               other_barry_posted = 1, last_outcome = excluded.last_outcome, updated_at = excluded.updated_at",
                        ),
                        "other_other_barry" => Some(
                            "INSERT INTO multi_review_runs
                              (repo_owner, repo_name, pr_number, head_sha, other_other_barry_posted, last_outcome, updated_at)
                             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6)
                             ON CONFLICT(repo_owner, repo_name, pr_number, head_sha) DO UPDATE SET
                               other_other_barry_posted = 1, last_outcome = excluded.last_outcome, updated_at = excluded.updated_at",
                        ),
                        _ => None,
                    };
                    let Some(sql) = sql else {
                        tracing::error!(%identity, "record_post called with unknown identity");
                        reply.send(Err(DbError::Database(sqlx::Error::Protocol(format!(
                            "unknown identity: {identity}"
                        )))));
                        continue;
                    };
                    let result = retry_busy(&rt, || {
                        let owner = key.owner.clone();
                        let repo = key.repo.clone();
                        let head_sha = key.head_sha.clone();
                        let outcome = outcome.clone();
                        async move {
                            sqlx::query(sql)
                                .bind(&owner)
                                .bind(&repo)
                                .bind(key.pr)
                                .bind(&head_sha)
                                .bind(&outcome)
                                .bind(now_ts)
                                .execute(unsafe { &mut *raw })
                                .await
                                .map(|_| ())
                        }
                    });
                    let duration_ms = start.elapsed().as_millis() as u64;
                    record_db_timing("record_post", duration_ms);
                    reply.send(result)
                }
                ActorCommand::RecordConferUsed { key, now_ts, reply } => {
                    let start = std::time::Instant::now();
                    let result = retry_busy(&rt, || {
                        let owner = key.owner.clone();
                        let repo = key.repo.clone();
                        let head_sha = key.head_sha.clone();
                        async move {
                            sqlx::query(
                                r#"INSERT INTO multi_review_runs
                                    (repo_owner, repo_name, pr_number, head_sha, confers_used, updated_at)
                                   VALUES (?1, ?2, ?3, ?4, 1, ?5)
                                   ON CONFLICT(repo_owner, repo_name, pr_number, head_sha) DO UPDATE SET
                                     confers_used = confers_used + 1, updated_at = excluded.updated_at"#,
                            )
                            .bind(&owner)
                            .bind(&repo)
                            .bind(key.pr)
                            .bind(&head_sha)
                            .bind(now_ts)
                            .execute(unsafe { &mut *raw })
                            .await
                            .map(|_| ())
                        }
                    });
                    let duration_ms = start.elapsed().as_millis() as u64;
                    record_db_timing("record_confer_used", duration_ms);
                    reply.send(result)
                }
                ActorCommand::RunState { key, reply } => {
                    let start = std::time::Instant::now();
                    let result = retry_busy(&rt, || {
                        let owner = key.owner.clone();
                        let repo = key.repo.clone();
                        let head_sha = key.head_sha.clone();
                        async move {
                            let row = sqlx::query(
                                r#"SELECT barry_posted, other_barry_posted, other_other_barry_posted,
                                          confers_used, last_outcome
                                     FROM multi_review_runs
                                     WHERE repo_owner=?1 AND repo_name=?2 AND pr_number=?3 AND head_sha=?4"#,
                            )
                            .bind(&owner)
                            .bind(&repo)
                            .bind(key.pr)
                            .bind(&head_sha)
                            .fetch_optional(unsafe { &mut *raw })
                            .await?;
                            Ok(row.map(|r| RunState {
                                barry_posted: r.get::<i64, _>("barry_posted") != 0,
                                other_barry_posted: r.get::<i64, _>("other_barry_posted") != 0,
                                other_other_barry_posted: r
                                    .get::<i64, _>("other_other_barry_posted")
                                    != 0,
                                confers_used: r.get::<i64, _>("confers_used") as u32,
                                last_outcome: r.get("last_outcome"),
                            }))
                        }
                    });
                    let duration_ms = start.elapsed().as_millis() as u64;
                    record_db_timing("run_state", duration_ms);
                    reply.send(result)
                }
                ActorCommand::AppendAudit { entry, reply } => {
                    let start = std::time::Instant::now();
                    let result = retry_busy(&rt, || {
                        let delivery_id = entry.delivery_id.clone();
                        let repo_owner = entry.repo_owner.clone();
                        let repo_name = entry.repo_name.clone();
                        let checker_name = entry.checker_name.clone();
                        let outcome = entry.outcome.clone();
                        let details = entry.details.clone();
                        async move {
                            sqlx::query(
                                r#"INSERT INTO audit_log
                                   (ts, delivery_id, repo_owner, repo_name, pr_number, checker_name, outcome, duration_ms, details)
                                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"#,
                            )
                            .bind(entry.ts)
                            .bind(delivery_id)
                            .bind(repo_owner)
                            .bind(repo_name)
                            .bind(entry.pr_number)
                            .bind(checker_name)
                            .bind(outcome)
                            .bind(entry.duration_ms)
                            .bind(details)
                            .execute(unsafe { &mut *raw })
                            .await
                            .map(|_| ())
                        }
                    });
                    let duration_ms = start.elapsed().as_millis() as u64;
                    record_db_timing("append_audit", duration_ms);
                    reply.send(result)
                }
                ActorCommand::CancelPrJobs {
                    repo_owner,
                    repo_name,
                    pr_number,
                    reply,
                } => {
                    let start = std::time::Instant::now();
                    let result = retry_busy(&rt, || {
                        let owner = repo_owner.clone();
                        let repo = repo_name.clone();
                        async move {
                            sqlx::query(
                                "DELETE FROM jobs \
                                 WHERE repo_owner=?1 AND repo_name=?2 AND pr_number=?3 \
                                   AND leased_until IS NULL",
                            )
                            .bind(owner)
                            .bind(repo)
                            .bind(pr_number)
                            .execute(unsafe { &mut *raw })
                            .await
                            .map(|_| ())
                        }
                    });
                    let duration_ms = start.elapsed().as_millis() as u64;
                    record_db_timing("cancel_pr_jobs", duration_ms);
                    reply.send(result)
                }
                ActorCommand::RawQuery { sql, reply } => {
                    let start = std::time::Instant::now();
                    let result = retry_busy(&rt, || {
                        let sql = sql.clone();
                        async move {
                            let rows = sqlx::query(&sql).fetch_all(unsafe { &mut *raw }).await?;
                            let mut all_rows = Vec::new();
                            for row in &rows {
                                let mut row_vals = Vec::new();
                                let cols = row.columns();
                                for col in cols.iter() {
                                    let name = col.name().trim_end_matches('$');
                                    // Probe NULL first, then try each storage class in
                                    // SQLite's native order. Decoding failure surfaces
                                    // as an error rather than silently substituting
                                    // an empty blob (which would corrupt test
                                    // assertions about row contents).
                                    let val = if matches!(
                                        row.try_get::<Option<i64>, _>(name),
                                        Ok(None)
                                    ) {
                                        RawSqliteValue::Null
                                    } else if let Ok(v) = row.try_get::<i64, _>(name) {
                                        RawSqliteValue::Integer(v)
                                    } else if let Ok(v) = row.try_get::<f64, _>(name) {
                                        RawSqliteValue::Real(v)
                                    } else if let Ok(v) = row.try_get::<String, _>(name) {
                                        RawSqliteValue::Text(v)
                                    } else if let Ok(v) = row.try_get::<Vec<u8>, _>(name) {
                                        RawSqliteValue::Blob(v)
                                    } else {
                                        return Err(sqlx::Error::ColumnDecode {
                                            index: name.to_string(),
                                            source: format!(
                                                "raw_query: no decoder matched column '{name}'"
                                            )
                                            .into(),
                                        });
                                    };
                                    row_vals.push((name.to_string(), val));
                                }
                                all_rows.push(row_vals);
                            }
                            Ok(all_rows)
                        }
                    });
                    let duration_ms = start.elapsed().as_millis() as u64;
                    record_db_timing("raw_query", duration_ms);
                    reply.send(result)
                }
            };
        }
    })
}
