CREATE TABLE IF NOT EXISTS jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    installation_id INTEGER NOT NULL,
    repo_owner TEXT NOT NULL,
    repo_name TEXT NOT NULL,
    pr_number INTEGER NOT NULL,
    event_kind TEXT NOT NULL,
    delivery_id TEXT NOT NULL,
    received_at INTEGER NOT NULL,
    run_after INTEGER NOT NULL,
    leased_until INTEGER,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT
);

-- Partial unique index: at most one pending job per (repo, pr, event_kind).
CREATE UNIQUE INDEX IF NOT EXISTS jobs_pending_unique
    ON jobs(repo_owner, repo_name, pr_number, event_kind)
    WHERE leased_until IS NULL;

CREATE INDEX IF NOT EXISTS jobs_due_idx ON jobs(run_after) WHERE leased_until IS NULL;

CREATE TABLE IF NOT EXISTS installation_tokens (
    installation_id INTEGER PRIMARY KEY,
    token TEXT NOT NULL,
    expires_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS audit_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts INTEGER NOT NULL,
    delivery_id TEXT,
    repo_owner TEXT,
    repo_name TEXT,
    pr_number INTEGER,
    checker_name TEXT,
    outcome TEXT NOT NULL,
    duration_ms INTEGER,
    details TEXT
);

CREATE INDEX IF NOT EXISTS audit_ts_idx ON audit_log(ts)
