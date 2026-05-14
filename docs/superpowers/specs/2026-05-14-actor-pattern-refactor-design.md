# Actor Pattern Refactor for Storage Layer

## Goal

Refactor `Store` from a thin wrapper around `SqlitePool` into a single-actor architecture: one Tokio task owns the database connection(s), all callers send commands over an `mpsc` channel, and writes are serialized at the Rust level. This eliminates the need for hand-crafted atomic workarounds (like `UPDATE...RETURNING` in `lease_next`) and makes it easier to add metrics/timing around DB operations.

## Why Now

- The multi-reviewer feature is adding new storage shapes (per-reviewer dedupe state in `multi_review_runs`)
- Easier to refactor before the storage surface area grows further
- Multiple consumers (workers, webhook server, token fetcher) all hit the same pool concurrently

## Architecture

### StoreActor

A single Tokio task spawned when `Store::open()` is called. It owns the SQLite connection lifecycle and runs a message loop:

```rust
async fn run(mut rx: mpsc::Receiver<Command>) {
    while let Some(Command { cmd, reply }) = rx.recv().await {
        match cmd {
            // ... match arm for each command variant
        }
    }
}
```

### Command Enum

Each `Store` method builds a `Command` variant with an `oneshot::Sender<Result<T, DbError>>` reply channel:

```rust
enum Command {
    LeaseNext { now_ts, lease_secs, reply: oneshot::Sender<Result<Option<LeasedJob>, DbError>> },
    Ack { job_id, reply: oneshot::Sender<Result<(), DbError>> },
    RescheduleAt { job_id, run_after, reason, reply: oneshot::Sender<Result<(), DbError>> },
    Nack { job_id, now_ts, error, max_attempts, backoff, reply: oneshot::Sender<Result<bool, DbError>> },
    Enqueue { job, now_ts, run_after, reply: oneshot::Sender<Result<(), DbError>> },
    PendingRunAfter { repo_owner, repo_name, pr, event_kind, reply: oneshot::Sender<Result<Option<i64>, DbError>> },
    GetTokenFor { identity, installation_id, now_ts, reply: oneshot::Sender<Result<Option<CachedToken>, DbError>> },
    PutTokenFor { identity, installation_id, token, expires_at, reply: oneshot::Sender<Result<(), DbError>> },
    GetToken { installation_id, now_ts, reply: oneshot::Sender<Result<Option<CachedToken>, DbError>> },
    PutToken { installation_id, token, expires_at, reply: oneshot::Sender<Result<(), DbError>> },
    RecordPost { key, identity, outcome, now_ts, reply: oneshot::Sender<Result<(), DbError>> },
    RecordConferUsed { key, now_ts, reply: oneshot::Sender<Result<(), DbError>> },
    RunState { key, reply: oneshot::Sender<Result<Option<RunState>, DbError>> },
    AppendAudit { entry, reply: oneshot::Sender<Result<(), DbError>> },
}
```

### DbError

Custom error type for actor-returned errors:

```rust
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("database busy, try again")]
    Busy,
    #[error("actor shut down")]
    Closed,
}
```

`DbError::Closed` is returned if the caller sends a command after the actor task has exited (e.g., during graceful shutdown). The actor spawns on `Store::open()` and runs until the `Store` is dropped (all senders dropped).

### Read Cache

Between the `Store` handle and the `mpsc::Sender`, a `ReadCache` middleware intercepts `GetTokenFor` commands. If a valid cached token exists (not expired, with 60s skew margin), it replies immediately without sending over the channel.

```rust
struct ReadCache {
    data: parking_lot::Mutex<HashMap<(String, i64), CachedToken>>,
}
```

Cache entries use the same TTL logic as the current code: `expires_at - 60 > now_ts`.

### Store Handle

```rust
pub struct Store {
    tx: Arc<mpsc::Sender<Command>>,
    cache: ReadCache,
}
```

The public API remains identical — callers don't know they're talking to an actor.

## Data Flow

### Write path (lease_next example)

1. Worker calls `store.lease_next(now_ts, lease_secs)`
2. `Store::lease_next` builds `Command::LeaseNext { ..., reply: oneshot::channel().0 }`
3. Command sent via `self.tx.send(Command { ... })`
4. Actor receives on `rx`, matches `Command::LeaseNext`, executes SQL
5. If `SQLITE_BUSY`, actor retries 3x with exponential backoff (100/200/400ms)
6. Actor sends `reply.send(result)`
7. `Store::lease_next` awaits reply and returns `Result<Option<LeasedJob>, DbError>`

### Read path with cache (token lookup)

1. Caller calls `store.get_installation_token_for(identity, installation_id, now_ts)`
2. `Store` checks `ReadCache::get()` — if valid cached token exists, returns immediately (no channel hop)
3. Cache miss: builds `Command::GetTokenFor`, sends to actor
4. Actor queries SQLite, returns result
5. On success, `Store` writes result into `ReadCache` for future calls

### Write with cache write-through (token store)

1. Caller calls `store.put_installation_token_for(identity, installation_id, token, expires_at)`
2. Builds `Command::PutTokenFor`, sends to actor
3. Actor executes SQL INSERT/UPDATE
4. On success, `Store` writes result into `ReadCache` (write-through)
5. Invalidates cache on update (token changed)

## File Structure

```
src/storage/
    mod.rs        - Store struct, DbError, Store::open(), Store::in_memory(), actor spawn
    actor.rs      - Command enum, StoreActor task, actor loop
    cache.rs      - ReadCache struct with get/put/invalidate
    queue.rs      - Rebuilt: uses Command builders instead of direct pool queries
    tokens.rs     - Rebuilt: uses cache middleware + Command builders
    audit.rs      - Rebuilt: uses Command builders
    multi_review.rs - Rebuilt: uses Command builders
    schema.sql    - Unchanged
```

## Error Handling: SQLITE Busy Retry

When a command fails with `sqlx::Error::DatabaseError` containing `SQLITE_BUSY`, the actor retries that specific command 3 times (4 total attempts: initial + 3 retries) with delays of 100ms, 200ms, 400ms (exponential). If all retries fail, `DbError::Busy` is sent back to the caller.

This is handled inside the actor loop, not by callers. Callers see a clean `Result<T, DbError>`.

## Testing Strategy

`Store::in_memory()` continues to work — it spawns an actor task with an in-memory SQLite pool (`sqlite::memory:`). All existing tests pass without modification since the public API is unchanged.

Test structure:
- `mod.rs` — tests actor spawns and schema migration works
- `cache.rs` — tests cache hit/miss/expiry/invalidation
- `queue.rs` — all existing lease/ack/nack/reschedule tests (unchanged assertions)
- `tokens.rs` — all existing token tests (unchanged assertions)
- `audit.rs` — existing append test (unchanged)
- `multi_review.rs` — all existing multi_review tests (unchanged)

## Migration Plan

1. Add `DbError` type to `mod.rs`
2. Create `actor.rs` with `Command` enum and actor loop skeleton
3. Create `cache.rs` with `ReadCache` struct
4. Refactor `Store` in `mod.rs` — replace `pool: SqlitePool` with `tx: Arc<mpsc::Sender<Command>>` and `cache: ReadCache`. The actor task owns the pool internally.
5. Update `queue.rs` impl blocks to build Commands instead of querying pool
6. Update `tokens.rs` impl blocks to use cache + Commands
7. Update `audit.rs` impl block to use Command
8. Update `multi_review.rs` impl blocks to use Commands
9. Update `in_memory()` to spawn an actor task
10. Run tests, fix any issues

## Out of Scope

- Switching to Postgres (SQLite-only by design, unchanged)
- Adding distributed tracing/metrics (future work, actor structure makes it easy to add)
- Connection pooling (single connection via actor simplifies concurrency)
- Changing the database schema
