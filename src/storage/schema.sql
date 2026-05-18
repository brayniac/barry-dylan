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
    last_error TEXT,
    -- GitHub login of the user who triggered the event. Set for issue_comment
    -- commands (the comment author), null for system events. Used to authorize
    -- /barry confer and /barry approve against the actual issuer rather than
    -- relying on a last-commenter heuristic.
    actor TEXT
);

-- Partial unique index: at most one pending job per (repo, pr, event_kind).
CREATE UNIQUE INDEX IF NOT EXISTS jobs_pending_unique
    ON jobs(repo_owner, repo_name, pr_number, event_kind)
    WHERE leased_until IS NULL;

CREATE INDEX IF NOT EXISTS jobs_due_idx ON jobs(run_after) WHERE leased_until IS NULL;

CREATE TABLE IF NOT EXISTS installation_tokens (
    installation_id INTEGER NOT NULL,
    identity TEXT NOT NULL DEFAULT 'barry',
    token TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    PRIMARY KEY (installation_id, identity)
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

CREATE INDEX IF NOT EXISTS audit_ts_idx ON audit_log(ts);

CREATE TABLE IF NOT EXISTS multi_review_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_owner TEXT NOT NULL,
    repo_name TEXT NOT NULL,
    pr_number INTEGER NOT NULL,
    head_sha TEXT NOT NULL,
    barry_posted INTEGER NOT NULL DEFAULT 0,
    other_barry_posted INTEGER NOT NULL DEFAULT 0,
    other_other_barry_posted INTEGER NOT NULL DEFAULT 0,
    confers_used INTEGER NOT NULL DEFAULT 0,
    last_outcome TEXT,
    updated_at INTEGER NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS multi_review_runs_unique
    ON multi_review_runs(repo_owner, repo_name, pr_number, head_sha);

CREATE TABLE IF NOT EXISTS installation_cache (
    identity        TEXT NOT NULL,
    owner           TEXT NOT NULL,
    installation_id INTEGER,
    cached_at       INTEGER NOT NULL,
    PRIMARY KEY (identity, owner)
);
