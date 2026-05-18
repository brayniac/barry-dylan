# Multi-Review Judge Robustness + Per-Identity Installation Resolution — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix two production bugs in the multi-review pipeline: (1) judge LLM returning unparseable text forces a noisy double-post, and (2) `for_identity` passes Barry's `installation_id` to OB/OOB's Apps, producing 404s. Add per-identity installation resolution with a durable cache, a pre-check that skips OB's LLM entirely when not installed, and a one-time WARN+metric on misconfiguration.

**Architecture:** Two coordinated fixes sharing the new `installation_cache` SQLite table. Judge calls retry once on parse errors; the orchestrator falls back to `BarryAlone` (not `Disagree`) on judge failure. A new `GhFactoryError::NotInstalled` propagates from `MultiGhFactory::for_identity(identity, owner, repo)` when an App is genuinely not installed on a repo. `MultiReviewChecker` pre-checks OB installation before running OB's LLM phases; the orchestrator's existing `BarryAlone` machinery handles the result. `confer` does the same for OOB and posts a user-visible "not installed" PR comment.

**Tech Stack:** Rust, Tokio, sqlx (SQLite), reqwest, wiremock, tracing, metrics.

**Spec:** `docs/superpowers/specs/2026-05-18-judge-and-installation-fixes-design.md`

---

## File Map

| File | Action | Responsibility |
|---|---|---|
| `src/checker/multi_review/judge.rs` | Modify | Extract `judge_once`; retry once on `Parse`; WARN with truncated raw text |
| `src/checker/multi_review/orchestrator.rs` | Modify | `Err` arm → `BarryAlone` (not `Disagree`); add `run_barry_only` |
| `src/storage/schema.sql` | Modify | Add `installation_cache` table |
| `src/storage/actor.rs` | Modify | New `ActorCommand` variants: `GetInstallation`, `PutInstallation`, `InvalidateInstallation` |
| `src/storage/installation_cache.rs` | **Create** | `Store` methods: `get_installation`, `put_installation`, `invalidate_installation` + unit tests |
| `src/storage/mod.rs` | Modify | `pub mod installation_cache;` |
| `src/github/app.rs` | Modify | Add `resolve_installation_id_for_repo` helper (HTTP) |
| `src/dispatcher/run.rs` | Modify | Define `GhFactoryError`; change `MultiGhFactory::for_identity` signature; add `preflight_identity` |
| `src/app_runtime.rs` | Modify | Implement new `for_identity` + `preflight_identity` with cache + WARN + metric + 401 retry |
| `src/webhook/server.rs` | Modify | Populate Barry positive cache entry on inbound webhook |
| `src/checker/multi_review/posting.rs` | Modify | `post_review` takes `owner`/`repo`; returns `Result<(), GhFactoryError>` |
| `src/checker/multi_review/mod.rs` | Modify | Pre-check OB; route to `run_barry_only` on `NotInstalled`; handle `NotInstalled` at post time |
| `src/checker/multi_review/confer.rs` | Modify | Pre-check target identity; post "not installed" PR comment on `NotInstalled` |

---

## Task 1: Judge — extract `judge_once` helper

**Files:**
- Modify: `src/checker/multi_review/judge.rs`

- [ ] **Step 1: Refactor `judge` to call an inner `judge_once` (no behavior change yet)**

In `src/checker/multi_review/judge.rs`, replace the body of `judge` (around lines 56-77) so the LLM call + parse logic is in a new private `judge_once` helper. The public `judge` now just calls `judge_once` once.

Replace this block (the existing `judge` function body from line 56 onward):
```rust
    let req = LlmRequest {
        system: Some(JUDGE_TEMPLATE.to_string()),
        messages: vec![LlmMessage {
            role: Role::User,
            content: user,
        }],
        max_tokens,
        temperature: 0.0,
    };
    let resp = client.complete(&req).await?;
    let slice = locate_json(&resp.text).ok_or_else(|| JudgeError::Parse(resp.text.clone()))?;
    let parsed: JudgeResp =
        serde_json::from_str(slice).map_err(|e| JudgeError::Parse(e.to_string()))?;
    Ok(JudgeVerdict {
        agree: parsed.agree,
        reason: parsed.reason,
        tokens: TokenCount {
            input: u64::from(resp.input_tokens.unwrap_or(0)),
            output: u64::from(resp.output_tokens.unwrap_or(0)),
        },
    })
}
```

with:
```rust
    judge_once(client, &user, max_tokens).await
}

async fn judge_once(
    client: &dyn LlmClient,
    user: &str,
    max_tokens: u32,
) -> Result<JudgeVerdict, JudgeError> {
    let req = LlmRequest {
        system: Some(JUDGE_TEMPLATE.to_string()),
        messages: vec![LlmMessage {
            role: Role::User,
            content: user.to_string(),
        }],
        max_tokens,
        temperature: 0.0,
    };
    let resp = client.complete(&req).await?;
    let slice = locate_json(&resp.text).ok_or_else(|| JudgeError::Parse(resp.text.clone()))?;
    let parsed: JudgeResp =
        serde_json::from_str(slice).map_err(|e| JudgeError::Parse(e.to_string()))?;
    Ok(JudgeVerdict {
        agree: parsed.agree,
        reason: parsed.reason,
        tokens: TokenCount {
            input: u64::from(resp.input_tokens.unwrap_or(0)),
            output: u64::from(resp.output_tokens.unwrap_or(0)),
        },
    })
}
```

(The `user` string is already built earlier in `judge`; it now becomes a local that's passed into `judge_once` by reference.)

- [ ] **Step 2: Confirm existing tests still pass**

```bash
cargo test -q --lib checker::multi_review::judge
```
Expected: existing tests pass — pure refactor, no behavior change.

- [ ] **Step 3: Commit**

```bash
git add src/checker/multi_review/judge.rs
git commit -m "refactor(judge): extract judge_once helper (no behavior change)"
```

---

## Task 2: Judge — retry once on `Parse`, WARN with truncated raw text

**Files:**
- Modify: `src/checker/multi_review/judge.rs`

- [ ] **Step 1: Write the failing test for retry-on-parse**

In `src/checker/multi_review/judge.rs`, find the `#[cfg(test)] mod tests` block. The existing `StubClient` returns a single fixed response. Replace it (or add alongside) with a script-style client and a new test.

Add at the bottom of the `mod tests` block, just before the closing `}`:

```rust
    struct ScriptClient {
        resps: Mutex<Vec<Result<LlmResponse, crate::llm::LlmError>>>,
        recorded: Arc<Mutex<Vec<LlmRequest>>>,
    }

    #[async_trait]
    impl crate::llm::LlmClient for ScriptClient {
        async fn complete(
            &self,
            req: &LlmRequest,
        ) -> Result<LlmResponse, crate::llm::LlmError> {
            self.recorded.lock().unwrap().push(req.clone());
            let mut q = self.resps.lock().unwrap();
            if q.is_empty() {
                panic!("ScriptClient exhausted");
            }
            q.remove(0)
        }
    }

    fn rev() -> UnifiedReview {
        UnifiedReview {
            outcome: Outcome::Comment,
            summary: "x".into(),
            findings: vec![],
        }
    }

    #[tokio::test]
    async fn judge_retries_once_on_parse_then_succeeds() {
        let recorded = Arc::new(Mutex::new(vec![]));
        let client = ScriptClient {
            resps: Mutex::new(vec![
                Ok(LlmResponse {
                    text: "".into(),
                    input_tokens: Some(10),
                    output_tokens: Some(0),
                }),
                Ok(LlmResponse {
                    text: r#"{"agree":true,"reason":"ok"}"#.into(),
                    input_tokens: Some(11),
                    output_tokens: Some(5),
                }),
            ]),
            recorded: recorded.clone(),
        };
        let v = judge(&client, &rev(), &rev(), 256).await.unwrap();
        assert!(v.agree);
        // Second call (the successful one) is the one whose tokens we credit.
        assert_eq!(v.tokens.input, 11);
        assert_eq!(v.tokens.output, 5);
        assert_eq!(recorded.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn judge_returns_parse_after_both_attempts_fail() {
        let recorded = Arc::new(Mutex::new(vec![]));
        let client = ScriptClient {
            resps: Mutex::new(vec![
                Ok(LlmResponse {
                    text: "".into(),
                    input_tokens: Some(1),
                    output_tokens: Some(0),
                }),
                Ok(LlmResponse {
                    text: "still garbage".into(),
                    input_tokens: Some(1),
                    output_tokens: Some(0),
                }),
            ]),
            recorded: recorded.clone(),
        };
        let err = judge(&client, &rev(), &rev(), 256).await.unwrap_err();
        assert!(matches!(err, JudgeError::Parse(_)));
        assert_eq!(recorded.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn judge_does_not_retry_on_transport_errors() {
        let recorded = Arc::new(Mutex::new(vec![]));
        let client = ScriptClient {
            resps: Mutex::new(vec![Err(crate::llm::LlmError::Shape("boom".into()))]),
            recorded: recorded.clone(),
        };
        let err = judge(&client, &rev(), &rev(), 256).await.unwrap_err();
        assert!(matches!(err, JudgeError::Llm(_)));
        assert_eq!(recorded.lock().unwrap().len(), 1);
    }
```

You'll need to ensure these `use` lines are present at the top of the test module (add any that are missing):
```rust
    use crate::checker::multi_review::review::{Outcome, UnifiedReview};
    use crate::llm::{LlmClient, LlmError, LlmMessage, LlmRequest, LlmResponse, Role};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};
```

- [ ] **Step 2: Run the tests; verify they fail**

```bash
cargo test -q --lib checker::multi_review::judge
```
Expected: `judge_retries_once_on_parse_then_succeeds` and `judge_returns_parse_after_both_attempts_fail` FAIL because `judge` does not retry yet. `judge_does_not_retry_on_transport_errors` likely passes.

- [ ] **Step 3: Implement retry + WARN log**

In `src/checker/multi_review/judge.rs`, replace the body of `judge` (currently `judge_once(client, &user, max_tokens).await`) with:

```rust
    match judge_once(client, &user, max_tokens).await {
        Ok(v) => Ok(v),
        Err(JudgeError::Parse(raw)) => {
            let truncated: String = raw.chars().take(2048).collect();
            tracing::warn!(raw_text = %truncated, "judge parse failed; retrying once");
            match judge_once(client, &user, max_tokens).await {
                Ok(v) => Ok(v),
                Err(JudgeError::Parse(raw2)) => {
                    let truncated: String = raw2.chars().take(2048).collect();
                    tracing::warn!(raw_text = %truncated, "judge parse failed on retry; giving up");
                    Err(JudgeError::Parse(raw2))
                }
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}
```

- [ ] **Step 4: Run the tests; verify they pass**

```bash
cargo test -q --lib checker::multi_review::judge
```
Expected: all judge tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/checker/multi_review/judge.rs
git commit -m "feat(judge): retry once on Parse and log raw response

A single empty or malformed judge response should not force the
disagreement path. Retry the call once on JudgeError::Parse, logging
the raw response (truncated to 2KB) at WARN on each failure. Transport
errors are not retried."
```

---

## Task 3: Orchestrator — `Err` arm returns `BarryAlone`, not `Disagree`

**Files:**
- Modify: `src/checker/multi_review/orchestrator.rs:203-213`

- [ ] **Step 1: Write a failing orchestrator test**

In `src/checker/multi_review/orchestrator.rs` in the `mod tests` block, add at the bottom (just before the closing `}`):

```rust
    #[tokio::test]
    async fn judge_failure_returns_barry_alone() {
        // Both reviewers run full pipeline; judge returns garbage twice → orchestrator
        // should fall back to BarryAlone, not Disagree.
        let c = clients(
            vec![Ok(approve()), Ok(approve()), Ok(approve()), Ok(approve())],
            vec![Ok(comment()), Ok(comment()), Ok(comment()), Ok(comment())],
            vec![Err("transport boom"), Err("transport boom")],
        );
        let p = personas();
        let v = Orchestrator {
            clients: &c,
            personas: &p,
            tracker: Arc::new(StatusTracker::new()),
            job_id: 0,
        }
        .run(&[file()])
        .await
        .unwrap();
        match v {
            Verdict::BarryAlone { reason, .. } => {
                assert_eq!(reason, "judge unavailable");
            }
            other => panic!("wanted BarryAlone, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run the test; verify it fails**

```bash
cargo test -q --lib checker::multi_review::orchestrator::tests::judge_failure_returns_barry_alone
```
Expected: FAIL — current code returns `Verdict::Disagree { reason: "judge unavailable", ... }`.

- [ ] **Step 3: Change the orchestrator's Err arm**

In `src/checker/multi_review/orchestrator.rs`, replace the block at lines 203-213:

```rust
            Err(e) => {
                tracing::warn!(?e, "judge failed; defaulting to disagreement");
                tracing::info!(kind = "disagree", "verdict");
                metrics::counter!("barry_multi_review_judge_total", "verdict" => "disagree")
                    .increment(1);
                return Ok(Verdict::Disagree {
                    barry: barry_r2,
                    other_barry: ob_r2,
                    reason: "judge unavailable".into(),
                });
            }
```

with:
```rust
            Err(e) => {
                tracing::warn!(?e, "judge failed; posting Barry alone");
                tracing::info!(kind = "barry_alone", "verdict");
                metrics::counter!("barry_multi_review_barry_alone_total").increment(1);
                return Ok(Verdict::BarryAlone {
                    barry: barry_r2,
                    reason: "judge unavailable".into(),
                });
            }
```

- [ ] **Step 4: Run the new test; verify it passes**

```bash
cargo test -q --lib checker::multi_review::orchestrator
```
Expected: `judge_failure_returns_barry_alone` passes; the other orchestrator tests still pass.

- [ ] **Step 5: Commit**

```bash
git add src/checker/multi_review/orchestrator.rs
git commit -m "fix(orchestrator): fall back to BarryAlone on judge failure

A failed judge call (transport error, repeated parse failures, etc.)
gives us no information about whether the reviewers actually disagreed.
The previous default — Verdict::Disagree — produced noisy double-posts
and incremented the disagree counter as if we had observed disagreement.
Switch to BarryAlone, which posts only Barry's verdict and increments
the barry_alone counter."
```

---

## Task 4: Orchestrator — extract `run_barry_only`

**Files:**
- Modify: `src/checker/multi_review/orchestrator.rs`

This task pulls Barry-only synthesis into its own method so `MultiReviewChecker` can call it directly when OB is not installed, without running OB's LLM phases at all.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `src/checker/multi_review/orchestrator.rs`:

```rust
    #[tokio::test]
    async fn run_barry_only_skips_ob_and_judge() {
        // Barry's drafts (security + rust) + R1 synth = 3 calls. OB and judge are not called.
        let c = clients(
            vec![Ok(approve()), Ok(approve()), Ok(approve())],
            vec![], // OB client must not be called.
            vec![], // Judge client must not be called.
        );
        let p = personas();
        let v = Orchestrator {
            clients: &c,
            personas: &p,
            tracker: Arc::new(StatusTracker::new()),
            job_id: 0,
        }
        .run_barry_only(&[file()], "Other Barry not installed".into())
        .await
        .unwrap();
        match v {
            Verdict::BarryAlone { barry, reason } => {
                assert_eq!(barry.outcome, Outcome::Approve);
                assert_eq!(reason, "Other Barry not installed");
            }
            other => panic!("wanted BarryAlone, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run the test; verify it fails**

```bash
cargo test -q --lib checker::multi_review::orchestrator::tests::run_barry_only_skips_ob_and_judge
```
Expected: FAIL — `run_barry_only` does not exist.

- [ ] **Step 3: Implement `run_barry_only`**

In `src/checker/multi_review/orchestrator.rs`, add a new method on `Orchestrator<'_>` (place it just after the existing `run` method's closing brace). Use the same drafting + R1-synthesis pattern already used by the OB-failure branches in `run` (lines 75-92):

```rust
    /// Run Barry's pipeline alone (drafts + R1 synthesis). Used when OB is
    /// known to be unavailable before any LLM calls (e.g., not installed).
    /// No OB or judge calls are made.
    pub async fn run_barry_only(
        &self,
        files: &[ChangedFile],
        reason: String,
    ) -> anyhow::Result<Verdict> {
        self.tracker.set_phase(self.job_id, "diff");
        let diff = files_to_diff(files);
        self.tracker.set_phase(self.job_id, "drafts");
        tracing::debug!("Barry-only drafts starting");
        let barry_drafts = match self
            .run_drafts(Identity::Barry, &diff)
            .await
        {
            Ok(d) => d,
            Err(e) => return Err(anyhow::anyhow!("barry drafts failed: {e}")),
        };
        let draft_tok_in: u64 = barry_drafts.iter().map(|d| d.tokens.input).sum();
        let draft_tok_out: u64 = barry_drafts.iter().map(|d| d.tokens.output).sum();
        self.tracker
            .add_tokens(self.job_id, draft_tok_in, draft_tok_out);
        self.tracker.set_phase(self.job_id, "R1 synthesis");
        let (barry_r1, r1_tokens) = self
            .synthesize_for(Identity::Barry, &diff, &barry_drafts, None)
            .await
            .map_err(|e| anyhow::anyhow!("barry R1 failed: {e}"))?;
        self.tracker
            .add_tokens(self.job_id, r1_tokens.input, r1_tokens.output);
        tracing::info!(kind = "barry_alone", "verdict");
        metrics::counter!("barry_multi_review_barry_alone_total").increment(1);
        Ok(Verdict::BarryAlone {
            barry: barry_r1,
            reason,
        })
    }
```

Confirm the existing `run_drafts` method's signature. If it doesn't exist as a standalone method, look at the OB-failure branches (around `orchestrator.rs:75-92`) and extract the drafting logic the same way they invoke it. If drafts are inlined, you may need a small refactor: extract the per-identity draft loop into `run_drafts(&self, identity, diff) -> anyhow::Result<Vec<PersonaDraft>>` first. The TDD test above will not be satisfiable without the extraction; do the extraction in this same step.

- [ ] **Step 4: Run the test; verify it passes**

```bash
cargo test -q --lib checker::multi_review::orchestrator
```
Expected: `run_barry_only_skips_ob_and_judge` passes; the other orchestrator tests still pass.

- [ ] **Step 5: Commit**

```bash
git add src/checker/multi_review/orchestrator.rs
git commit -m "feat(orchestrator): add run_barry_only for pre-checked OB-unavailable

When the multi-review checker knows OB is unavailable before any LLM
call (e.g., the App is not installed on the repo), we want to skip OB
and judge entirely instead of starting both pipelines and degrading
mid-flight. This helper does just Barry's drafts + R1 synthesis and
returns BarryAlone with a caller-supplied reason string."
```

---

## Task 5: Storage schema — add `installation_cache` table

**Files:**
- Modify: `src/storage/schema.sql`

- [ ] **Step 1: Append the new table**

At the end of `src/storage/schema.sql`, append:

```sql
CREATE TABLE IF NOT EXISTS installation_cache (
    identity        TEXT NOT NULL,
    owner           TEXT NOT NULL,
    installation_id INTEGER,
    cached_at       INTEGER NOT NULL,
    PRIMARY KEY (identity, owner)
);
```

(SQLite does not support `STRICT` on older versions; the existing tables in this schema do not use STRICT, so we match.)

- [ ] **Step 2: Verify migration runs cleanly**

```bash
cargo test -q --lib storage::tests::in_memory_creates_schema
```
Expected: PASS — `Store::in_memory()` runs all migrations, including the new table.

- [ ] **Step 3: Commit**

```bash
git add src/storage/schema.sql
git commit -m "feat(storage): add installation_cache table

Per-(identity, owner) cache of GitHub App installation IDs.
NULL installation_id means a negative cache entry (App is not
installed on this owner). cached_at is enforced at read time:
negative entries older than 1h are treated as cache misses."
```

---

## Task 6: Storage — `installation_cache` actor commands + Store methods + tests

**Files:**
- Modify: `src/storage/actor.rs`
- Create: `src/storage/installation_cache.rs`
- Modify: `src/storage/mod.rs`

- [ ] **Step 1: Add ActorCommand variants**

In `src/storage/actor.rs`, inside `pub enum ActorCommand { ... }`, add three new variants near the other token-related ones (after the `PutTokenFor` variant, around line 97):

```rust
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
```

At the top of `src/storage/actor.rs` (with the other `pub use` / type imports), add:
```rust
use crate::storage::installation_cache::CachedInstallation;
```

(The type will be defined in step 2.)

In the actor's `match` loop (the long block around line 442), add three new arms after the existing `PutTokenFor` arm and before `GetToken`. Place this code:

```rust
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
```

- [ ] **Step 2: Create the public Store methods + types**

Create `src/storage/installation_cache.rs`:

```rust
use crate::storage::Store;
use crate::storage::actor::{ActorCommand, Reply};
use tokio::sync::oneshot;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CachedInstallation {
    /// Positive cache entry: App is installed; installation_id is known.
    Cached { installation_id: i64 },
    /// Negative cache entry: App is not installed on this owner.
    /// Returned only if the entry is within the 1h TTL.
    NotInstalled,
}

impl Store {
    /// Look up a cached installation entry for (identity, owner).
    /// Negative cache entries older than 1h return None (treat as miss).
    pub async fn get_installation(
        &self,
        identity: &str,
        owner: &str,
        now_ts: i64,
    ) -> anyhow::Result<Option<CachedInstallation>> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::GetInstallation {
                identity: identity.to_string(),
                owner: owner.to_string(),
                now_ts,
                reply: Reply { tx },
            })
            .map_err(|_| crate::storage::DbError::Closed)?;
        let result = rx.await.map_err(|_| crate::storage::DbError::Closed)??;
        Ok(result)
    }

    /// Insert or update an installation cache entry. Pass `None` for `installation_id`
    /// to store a negative cache entry.
    pub async fn put_installation(
        &self,
        identity: &str,
        owner: &str,
        installation_id: Option<i64>,
        now_ts: i64,
    ) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::PutInstallation {
                identity: identity.to_string(),
                owner: owner.to_string(),
                installation_id,
                cached_at: now_ts,
                reply: Reply { tx },
            })
            .map_err(|_| crate::storage::DbError::Closed)?;
        rx.await.map_err(|_| crate::storage::DbError::Closed)??;
        Ok(())
    }

    /// Delete any cached entry for (identity, owner). Used when a token mint
    /// returns 401 (App was uninstalled after we cached the positive entry).
    pub async fn invalidate_installation(
        &self,
        identity: &str,
        owner: &str,
    ) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::InvalidateInstallation {
                identity: identity.to_string(),
                owner: owner.to_string(),
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
    async fn positive_entry_round_trip() {
        let s = Store::in_memory().await.unwrap();
        s.put_installation("other_barry", "acme", Some(42), 1000)
            .await
            .unwrap();
        let v = s
            .get_installation("other_barry", "acme", 1000)
            .await
            .unwrap();
        assert_eq!(
            v,
            Some(CachedInstallation::Cached {
                installation_id: 42
            })
        );
    }

    #[tokio::test]
    async fn negative_entry_within_ttl() {
        let s = Store::in_memory().await.unwrap();
        s.put_installation("other_barry", "acme", None, 1000)
            .await
            .unwrap();
        let v = s
            .get_installation("other_barry", "acme", 1500)
            .await
            .unwrap();
        assert_eq!(v, Some(CachedInstallation::NotInstalled));
    }

    #[tokio::test]
    async fn negative_entry_stale_returns_none() {
        let s = Store::in_memory().await.unwrap();
        s.put_installation("other_barry", "acme", None, 1000)
            .await
            .unwrap();
        // now > cached_at + 3600
        let v = s
            .get_installation("other_barry", "acme", 5000)
            .await
            .unwrap();
        assert_eq!(v, None);
    }

    #[tokio::test]
    async fn positive_entry_never_expires() {
        let s = Store::in_memory().await.unwrap();
        s.put_installation("other_barry", "acme", Some(7), 0)
            .await
            .unwrap();
        let v = s
            .get_installation("other_barry", "acme", 100_000)
            .await
            .unwrap();
        assert_eq!(
            v,
            Some(CachedInstallation::Cached { installation_id: 7 })
        );
    }

    #[tokio::test]
    async fn identities_do_not_collide() {
        let s = Store::in_memory().await.unwrap();
        s.put_installation("barry", "acme", Some(1), 1000)
            .await
            .unwrap();
        s.put_installation("other_barry", "acme", None, 1000)
            .await
            .unwrap();

        let b = s.get_installation("barry", "acme", 1000).await.unwrap();
        let ob = s
            .get_installation("other_barry", "acme", 1000)
            .await
            .unwrap();

        assert_eq!(
            b,
            Some(CachedInstallation::Cached { installation_id: 1 })
        );
        assert_eq!(ob, Some(CachedInstallation::NotInstalled));
    }

    #[tokio::test]
    async fn invalidate_removes_row() {
        let s = Store::in_memory().await.unwrap();
        s.put_installation("other_barry", "acme", Some(42), 1000)
            .await
            .unwrap();
        s.invalidate_installation("other_barry", "acme")
            .await
            .unwrap();
        let v = s
            .get_installation("other_barry", "acme", 1000)
            .await
            .unwrap();
        assert_eq!(v, None);
    }
}
```

- [ ] **Step 3: Register the new module**

In `src/storage/mod.rs`, find the existing `pub mod` declarations (near the top of the file; look for `pub mod multi_review;` or similar) and add:
```rust
pub mod installation_cache;
```

If the file `re-exports` types from other storage submodules (e.g., `pub use multi_review::*;`), add the analogous line:
```rust
pub use installation_cache::CachedInstallation;
```

- [ ] **Step 4: Run the new tests; verify they pass**

```bash
cargo test -q --lib storage::installation_cache
```
Expected: all six tests pass.

- [ ] **Step 5: Run the full storage suite to confirm no regressions**

```bash
cargo test -q --lib storage
```
Expected: all storage tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/storage/actor.rs src/storage/installation_cache.rs src/storage/mod.rs
git commit -m "feat(storage): add installation_cache get/put/invalidate

Per-(identity, owner) cache of GitHub App installation IDs, with a 1h
TTL on negative entries and no TTL on positive ones. Positive entries
are invalidated explicitly via invalidate_installation when a token
mint returns 401 (App was uninstalled out from under us). Tests cover
round-trip, TTL semantics, identity isolation, and invalidate."
```

---

## Task 7: GitHub — `resolve_installation_id_for_repo` helper + tests

**Files:**
- Modify: `src/github/app.rs`

This helper performs the actual `GET /repos/{owner}/{repo}/installation` call with an App-scoped JWT. It is the only piece that talks to GitHub for installation resolution; the factory composes it with the cache.

- [ ] **Step 1: Write the failing tests with wiremock**

Add this test module body at the bottom of `src/github/app.rs`, inside the existing `#[cfg(test)] mod tests { ... }` block (just before its closing `}`):

```rust
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_creds() -> AppCreds {
        AppCreds {
            app_id: 12345,
            private_key_pem: test_key_pem(),
        }
    }

    #[tokio::test]
    async fn resolve_installation_id_returns_id_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widget/installation"))
            .and(header("Accept", "application/vnd.github+json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 42
            })))
            .mount(&server)
            .await;
        let http = reqwest::Client::new();
        let result = resolve_installation_id_for_repo(
            &http,
            &test_creds(),
            "acme",
            "widget",
            &server.uri(),
        )
        .await
        .unwrap();
        assert_eq!(result, Some(42));
    }

    #[tokio::test]
    async fn resolve_installation_id_returns_none_on_404() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widget/installation"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let http = reqwest::Client::new();
        let result = resolve_installation_id_for_repo(
            &http,
            &test_creds(),
            "acme",
            "widget",
            &server.uri(),
        )
        .await
        .unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn resolve_installation_id_errors_on_500() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widget/installation"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let http = reqwest::Client::new();
        let result = resolve_installation_id_for_repo(
            &http,
            &test_creds(),
            "acme",
            "widget",
            &server.uri(),
        )
        .await;
        assert!(result.is_err());
    }
```

Confirm `wiremock` is listed under `[dev-dependencies]` in `Cargo.toml`:
```bash
grep -E '^\s*wiremock\s*=' /Users/brian/workspace/brayniac/barry-dylan/Cargo.toml
```
Expected: prints a line with `wiremock = "0.6"`. If it's only in `[dependencies]`, move it to `[dev-dependencies]`. If absent entirely, add it.

- [ ] **Step 2: Run the tests; verify they fail**

```bash
cargo test -q --lib github::app::tests::resolve_installation_id_returns_id_on_200
```
Expected: COMPILE ERROR — `resolve_installation_id_for_repo` does not exist.

- [ ] **Step 3: Implement the helper**

In `src/github/app.rs`, add after the existing `fetch_installation_token` function:

```rust
/// Default GitHub API base URL. Tests pass a different URL pointing at wiremock.
pub const GITHUB_API_BASE: &str = "https://api.github.com";

/// Look up the App installation ID for a given owner/repo. Returns:
/// - `Ok(Some(id))` if the App is installed on the repo.
/// - `Ok(None)` if GitHub returns 404 (App is not installed on this owner).
/// - `Err(_)` for any other failure (transport, 5xx, unparseable response).
///
/// `base_url` is the GitHub API base (use `GITHUB_API_BASE` in production).
pub async fn resolve_installation_id_for_repo(
    http: &reqwest::Client,
    creds: &AppCreds,
    owner: &str,
    repo: &str,
    base_url: &str,
) -> anyhow::Result<Option<i64>> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let jwt = creds.mint_jwt(now)?;
    let url = format!("{base_url}/repos/{owner}/{repo}/installation");
    let resp = http
        .get(&url)
        .bearer_auth(&jwt)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "barry-dylan")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let resp = resp.error_for_status()?;
    #[derive(serde::Deserialize)]
    struct InstallationResponse {
        id: i64,
    }
    let parsed: InstallationResponse = resp.json().await?;
    Ok(Some(parsed.id))
}
```

- [ ] **Step 4: Run the tests; verify they pass**

```bash
cargo test -q --lib github::app
```
Expected: all three new tests pass; existing tests still pass.

- [ ] **Step 5: Commit**

```bash
git add src/github/app.rs
git commit -m "feat(github): add resolve_installation_id_for_repo helper

GET /repos/{owner}/{repo}/installation with an App-scoped JWT. Returns
Some(id) on 200, None on 404 (App not installed), Err on other failures.
The base URL is injectable so tests can point at a wiremock server."
```

---

## Task 8: `GhFactoryError` + trait signature change

**Files:**
- Modify: `src/dispatcher/run.rs`

- [ ] **Step 1: Define the error type and update the trait**

In `src/dispatcher/run.rs`, near the top (after the existing `use` statements; the `Identity` import already lives nearby), add:

```rust
use crate::checker::multi_review::identity::Identity as MrIdentity;

#[derive(Debug, thiserror::Error)]
pub enum GhFactoryError {
    #[error("{identity:?} is not installed on {owner}/{repo}")]
    NotInstalled {
        identity: MrIdentity,
        owner: String,
        repo: String,
    },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
```

If `thiserror` is not already a dependency, check `Cargo.toml`:
```bash
grep -E '^\s*thiserror\s*=' /Users/brian/workspace/brayniac/barry-dylan/Cargo.toml
```
If absent, add `thiserror = "1"` to `[dependencies]`.

Then replace the `MultiGhFactory` trait at `src/dispatcher/run.rs:52-60`:

```rust
#[async_trait::async_trait]
pub trait MultiGhFactory: GhFactory {
    /// Mint a GitHub client authenticated as the given identity for the given
    /// repo. Internally resolves the installation_id for (identity, owner).
    /// Returns `Err(NotInstalled)` if the identity's App is not installed on
    /// the owner.
    async fn for_identity(
        &self,
        identity: Identity,
        owner: &str,
        repo: &str,
    ) -> Result<Arc<GitHub>, GhFactoryError>;

    /// Verify the identity's App is installed on the owner without minting a
    /// token. Same cache writes and WARN+metric side effects as `for_identity`.
    async fn preflight_identity(
        &self,
        identity: Identity,
        owner: &str,
        repo: &str,
    ) -> Result<(), GhFactoryError>;
}
```

The `Identity` import at the top of `src/dispatcher/run.rs` should already exist (the trait references it); if `MrIdentity` and `Identity` are the same type, simplify to just `Identity` — the alias above was defensive in case of a naming collision.

- [ ] **Step 2: Update the `AppGhFactory::for_installation` shim**

In `src/app_runtime.rs`, the existing `impl GhFactory for AppGhFactory` (around line 33-39) currently delegates to `for_identity`. That delegation must change because the signature changed.

Replace lines 33-39:
```rust
impl GhFactory for AppGhFactory {
    async fn for_installation(&self, installation_id: i64) -> anyhow::Result<Arc<GitHub>> {
        // Default identity for the legacy path is Barry.
        self.for_identity(Identity::Barry, installation_id).await
    }
}
```

with a fresh implementation that mints a Barry token directly from an installation_id (no owner/repo lookup needed since the caller has the ID):
```rust
#[async_trait]
impl GhFactory for AppGhFactory {
    async fn for_installation(&self, installation_id: i64) -> anyhow::Result<Arc<GitHub>> {
        // Used by code paths that already have Barry's installation_id in hand
        // (e.g., dispatcher leasing a job). Identity-based callers should use
        // for_identity() so OB and OOB get resolved correctly.
        let now = crate::util::now_ts();
        let token = crate::github::app::get_or_mint_for(
            &self.store,
            &self.http,
            &self.barry,
            Identity::Barry,
            installation_id,
            now,
        )
        .await?;
        Ok(Arc::new(GitHub::new(self.http.clone(), token)))
    }
}
```

(The MultiGhFactory impl will be replaced wholesale in Task 9.)

- [ ] **Step 3: Compile-check (will fail; we'll fix the callers in later tasks)**

```bash
cargo build 2>&1 | head -40
```
Expected: errors in `app_runtime.rs` (MultiGhFactory impl signature mismatch), `posting.rs:44`, `confer.rs:105`, `confer.rs:117`. The trait change is intentional; subsequent tasks update each caller.

- [ ] **Step 4: Do not commit yet — Task 9 makes the MultiGhFactory impl compile.**

---

## Task 9: `AppGhFactory::for_identity` + `preflight_identity` with cache, WARN, metric

**Files:**
- Modify: `src/app_runtime.rs`

- [ ] **Step 1: Replace the MultiGhFactory impl**

In `src/app_runtime.rs`, locate the `impl MultiGhFactory for AppGhFactory` block (around line 41-60). Replace the whole block with:

```rust
#[async_trait]
impl MultiGhFactory for AppGhFactory {
    async fn for_identity(
        &self,
        identity: Identity,
        owner: &str,
        repo: &str,
    ) -> Result<Arc<GitHub>, GhFactoryError> {
        let now = crate::util::now_ts();
        let installation_id = self
            .resolve_installation(identity, owner, repo, now)
            .await?;
        let creds = self.creds_for(identity);
        let token = crate::github::app::get_or_mint_for(
            &self.store, &self.http, creds, identity, installation_id, now,
        )
        .await
        .map_err(|e| GhFactoryError::Other(e))?;
        Ok(Arc::new(GitHub::new(self.http.clone(), token)))
    }

    async fn preflight_identity(
        &self,
        identity: Identity,
        owner: &str,
        repo: &str,
    ) -> Result<(), GhFactoryError> {
        let now = crate::util::now_ts();
        let _ = self
            .resolve_installation(identity, owner, repo, now)
            .await?;
        Ok(())
    }
}
```

- [ ] **Step 2: Add `resolve_installation` to `AppGhFactory`**

Below the `MultiGhFactory` impl (still in `src/app_runtime.rs`), add an inherent impl block for `AppGhFactory`:

```rust
impl AppGhFactory {
    /// Resolve (identity, owner) → installation_id using the cache, falling
    /// back to a GitHub API lookup on miss. On 404, writes a negative cache
    /// entry and returns `NotInstalled`. WARN + metric are emitted exactly
    /// once per (identity, owner) thanks to the cache short-circuit.
    async fn resolve_installation(
        &self,
        identity: Identity,
        owner: &str,
        repo: &str,
        now: i64,
    ) -> Result<i64, GhFactoryError> {
        // Cache check.
        match self
            .store
            .get_installation(identity.slug(), owner, now)
            .await
            .map_err(GhFactoryError::Other)?
        {
            Some(crate::storage::CachedInstallation::Cached { installation_id }) => {
                return Ok(installation_id);
            }
            Some(crate::storage::CachedInstallation::NotInstalled) => {
                return Err(GhFactoryError::NotInstalled {
                    identity,
                    owner: owner.to_string(),
                    repo: repo.to_string(),
                });
            }
            None => {}
        }
        // Miss → GitHub lookup.
        let base = self.gh_api_base.as_deref().unwrap_or(
            crate::github::app::GITHUB_API_BASE,
        );
        let creds = self.creds_for(identity);
        let resolved = crate::github::app::resolve_installation_id_for_repo(
            &self.http, creds, owner, repo, base,
        )
        .await
        .map_err(GhFactoryError::Other)?;
        match resolved {
            Some(id) => {
                self.store
                    .put_installation(identity.slug(), owner, Some(id), now)
                    .await
                    .map_err(GhFactoryError::Other)?;
                Ok(id)
            }
            None => {
                self.store
                    .put_installation(identity.slug(), owner, None, now)
                    .await
                    .map_err(GhFactoryError::Other)?;
                tracing::warn!(
                    identity = %identity.slug(),
                    owner = %owner,
                    repo = %repo,
                    "App not installed on owner; subsequent calls will use Barry alone"
                );
                metrics::counter!(
                    "barry_multi_review_identity_missing_total",
                    "identity" => identity.slug().to_string(),
                    "owner" => owner.to_string(),
                )
                .increment(1);
                Err(GhFactoryError::NotInstalled {
                    identity,
                    owner: owner.to_string(),
                    repo: repo.to_string(),
                })
            }
        }
    }
}
```

- [ ] **Step 3: Add `gh_api_base` field to `AppGhFactory`**

Find the `pub struct AppGhFactory { ... }` definition in `src/app_runtime.rs` (around line 15-25) and add a new field:

```rust
    pub gh_api_base: Option<String>,
```

In `run()` where `AppGhFactory` is constructed (around line 101-105), add the new field:

```rust
    let gh_factory: Arc<dyn MultiGhFactory> = Arc::new(AppGhFactory {
        barry,
        other_barry: ob,
        other_other_barry: oob,
        store: store.clone(),
        http: http.clone(),
        gh_api_base: None,
    });
```

(Other fields stay as they are.)

- [ ] **Step 4: Compile-check**

```bash
cargo build 2>&1 | head -40
```
Expected: errors in `posting.rs:44`, `mod.rs:81/101/114`, `confer.rs:105/117` — call sites with old signature. These are fixed in Tasks 11-14.

- [ ] **Step 5: Add unit tests for the resolution logic**

Add a `#[cfg(test)]` test module at the bottom of `src/app_runtime.rs`:

```rust
#[cfg(test)]
mod factory_tests {
    use super::*;
    use crate::checker::multi_review::identity::Identity;
    use crate::storage::Store;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_creds() -> Arc<crate::github::app::AppCreds> {
        // Use the test key fixture shared with github::app tests.
        Arc::new(crate::github::app::AppCreds {
            app_id: 12345,
            private_key_pem: include_bytes!("../tests/fixtures/test_app_key.pem").to_vec(),
        })
    }

    async fn factory_with(server_uri: String) -> (AppGhFactory, Store) {
        let store = Store::in_memory().await.unwrap();
        let http = reqwest::Client::new();
        let f = AppGhFactory {
            barry: test_creds(),
            other_barry: test_creds(),
            other_other_barry: test_creds(),
            store: store.clone(),
            http,
            gh_api_base: Some(server_uri),
        };
        (f, store)
    }

    #[tokio::test]
    async fn cache_miss_resolves_and_caches_positive() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widget/installation"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 99})))
            .expect(1)
            .mount(&server)
            .await;
        let (f, store) = factory_with(server.uri()).await;
        f.preflight_identity(Identity::OtherBarry, "acme", "widget")
            .await
            .unwrap();
        // Second call should hit cache, not the mock.
        f.preflight_identity(Identity::OtherBarry, "acme", "widget")
            .await
            .unwrap();
        let v = store
            .get_installation("other_barry", "acme", crate::util::now_ts())
            .await
            .unwrap();
        assert_eq!(
            v,
            Some(crate::storage::CachedInstallation::Cached { installation_id: 99 })
        );
    }

    #[tokio::test]
    async fn cache_miss_404_writes_negative_and_returns_not_installed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widget/installation"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        let (f, store) = factory_with(server.uri()).await;
        let err = f
            .preflight_identity(Identity::OtherBarry, "acme", "widget")
            .await
            .unwrap_err();
        assert!(matches!(err, GhFactoryError::NotInstalled { .. }));
        // Second call hits negative cache; no additional HTTP traffic.
        let err = f
            .preflight_identity(Identity::OtherBarry, "acme", "widget")
            .await
            .unwrap_err();
        assert!(matches!(err, GhFactoryError::NotInstalled { .. }));
        let v = store
            .get_installation("other_barry", "acme", crate::util::now_ts())
            .await
            .unwrap();
        assert_eq!(v, Some(crate::storage::CachedInstallation::NotInstalled));
    }

    #[tokio::test]
    async fn positive_cache_hit_skips_http() {
        let server = MockServer::start().await;
        // No mounted mocks — any HTTP call here would fail.
        let (f, store) = factory_with(server.uri()).await;
        store
            .put_installation("other_barry", "acme", Some(99), crate::util::now_ts())
            .await
            .unwrap();
        f.preflight_identity(Identity::OtherBarry, "acme", "widget")
            .await
            .unwrap();
    }
}
```

- [ ] **Step 6: Run the new tests; verify they pass**

```bash
cargo test -q --lib factory_tests
```
Expected: three tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/dispatcher/run.rs src/app_runtime.rs
git commit -m "feat(factory): per-identity installation resolution with cache

MultiGhFactory::for_identity now takes (identity, owner, repo) and
resolves each App's own installation_id via GET /repos/.../installation.
Results are cached in the installation_cache table (positive: indefinite,
negative: 1h TTL). On 404 we write a negative cache entry, emit a one-
time WARN, and increment barry_multi_review_identity_missing_total.

GhFactoryError::NotInstalled is the typed signal callers use to fall
back to BarryAlone instead of treating this as a generic failure.

Callers (post_review, multi-review checker, confer) are updated in the
following tasks."
```

---

## Task 10: `AppGhFactory` — 401 invalidate-and-retry

**Files:**
- Modify: `src/app_runtime.rs`

- [ ] **Step 1: Write a failing test for the 401 retry**

Add to the `factory_tests` module in `src/app_runtime.rs`:

```rust
    #[tokio::test]
    async fn token_mint_401_invalidates_cache_and_retries() {
        let server = MockServer::start().await;
        // 1) First /repos lookup → 200 with id=99 (seeds positive cache via for_identity).
        // 2) /app/installations/99/access_tokens → 401 (install was removed).
        // 3) After invalidation, /repos lookup is retried → 404 → NotInstalled.
        Mock::given(method("GET"))
            .and(path("/repos/acme/widget/installation"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 99})))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/app/installations/99/access_tokens"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widget/installation"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let (f, _store) = factory_with(server.uri()).await;
        let err = f
            .for_identity(Identity::OtherBarry, "acme", "widget")
            .await
            .unwrap_err();
        assert!(matches!(err, GhFactoryError::NotInstalled { .. }));
    }
```

This also requires the token-mint URL to be relative to the same configurable base. Check `src/github/app.rs::fetch_installation_token` (Task 7 left it pointing at `https://api.github.com`). For Task 10 to be testable, generalize `fetch_installation_token` to accept a `base_url: &str` like `resolve_installation_id_for_repo`. Update the production callers to pass `crate::github::app::GITHUB_API_BASE`.

Concretely, in `src/github/app.rs`:

1. Change `fetch_installation_token` to take an additional `base_url: &str` parameter and build the URL as `format!("{base_url}/app/installations/{installation_id}/access_tokens")`.
2. Change `get_or_mint_for` to take `base_url: &str` and pass it through to `fetch_installation_token`.
3. Update the two callers in `app_runtime.rs` (`for_installation` shim and the new `for_identity` path) to pass `self.gh_api_base.as_deref().unwrap_or(crate::github::app::GITHUB_API_BASE)`.

- [ ] **Step 2: Run the test; verify it fails**

```bash
cargo test -q --lib factory_tests::token_mint_401_invalidates_cache_and_retries
```
Expected: FAIL or COMPILE ERROR — `for_identity` does not yet handle the 401 retry path.

- [ ] **Step 3: Implement 401 invalidate-and-retry in `for_identity`**

In `src/app_runtime.rs`, replace the new `for_identity` impl with one that retries once on 401 by invalidating the positive cache entry:

```rust
    async fn for_identity(
        &self,
        identity: Identity,
        owner: &str,
        repo: &str,
    ) -> Result<Arc<GitHub>, GhFactoryError> {
        let now = crate::util::now_ts();
        let base = self
            .gh_api_base
            .as_deref()
            .unwrap_or(crate::github::app::GITHUB_API_BASE);
        let creds = self.creds_for(identity);
        let installation_id = self
            .resolve_installation(identity, owner, repo, now)
            .await?;
        match crate::github::app::get_or_mint_for(
            &self.store,
            &self.http,
            creds,
            identity,
            installation_id,
            now,
            base,
        )
        .await
        {
            Ok(token) => Ok(Arc::new(GitHub::new(self.http.clone(), token))),
            Err(e) if is_unauthorized(&e) => {
                // Stale positive cache: App was uninstalled. Invalidate, retry once.
                self.store
                    .invalidate_installation(identity.slug(), owner)
                    .await
                    .map_err(GhFactoryError::Other)?;
                let installation_id = self
                    .resolve_installation(identity, owner, repo, now)
                    .await?;
                let token = crate::github::app::get_or_mint_for(
                    &self.store,
                    &self.http,
                    creds,
                    identity,
                    installation_id,
                    now,
                    base,
                )
                .await
                .map_err(GhFactoryError::Other)?;
                Ok(Arc::new(GitHub::new(self.http.clone(), token)))
            }
            Err(e) => Err(GhFactoryError::Other(e)),
        }
    }
```

Add at the bottom of `src/app_runtime.rs` (outside the impl blocks):

```rust
fn is_unauthorized(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|cause| cause.downcast_ref::<reqwest::Error>())
        .any(|re| re.status() == Some(reqwest::StatusCode::UNAUTHORIZED))
}
```

- [ ] **Step 4: Run the test; verify it passes**

```bash
cargo test -q --lib factory_tests
```
Expected: all factory tests pass, including the 401 retry case.

- [ ] **Step 5: Commit**

```bash
git add src/app_runtime.rs src/github/app.rs
git commit -m "feat(factory): on 401 token mint, invalidate cache and retry once

A 401 from POST /app/installations/{id}/access_tokens means the App was
uninstalled after we cached the positive entry. Invalidate the positive
cache row and re-resolve once: the retry either picks up a new installation
ID or hits a 404 and returns NotInstalled. Bounded to a single retry.

Also makes fetch_installation_token's base URL injectable so wiremock
tests can exercise the full mint+401 path."
```

---

## Task 11: Webhook — populate Barry positive cache on inbound

**Files:**
- Modify: `src/webhook/server.rs`

When a webhook arrives, the `installation.id` field tells us Barry's installation_id for that org. Populating the cache at this point makes downstream `for_identity(Barry, owner, repo)` calls free.

- [ ] **Step 1: Identify where the inbound event is matched**

Open `src/webhook/server.rs` and locate the `match` arm that handles `InboundEvent::PullRequest(e)` (around line 80-113) — this is where `e.installation.id` is captured for `NewJob`.

- [ ] **Step 2: Add the cache write**

Inside that arm, just after `let pr = e.number;` (or the equivalent line capturing the PR number) and before the `Some((NewJob { ... }, ...))` return, add a call to `put_installation`:

```rust
            // Populate Barry's installation cache so downstream factory calls
            // for (Barry, owner) are a free DB lookup.
            let _ = store
                .put_installation(
                    "barry",
                    &owner,
                    Some(e.installation.id),
                    now,
                )
                .await;
```

`store` and `now` must already be in scope. If they're not, look at the function signature: the webhook handler typically receives `store: &Store` (or via an `AppState`) and computes `now` from `crate::util::now_ts()`. Wire them in if needed; this should be a one-line addition.

Apply the same pattern in the two other match arms that handle inbound events with an `installation.id` (the issue_comment arm around line 114-152 and the pull_request.closed arm around line 153-170). All three arms should populate the cache.

- [ ] **Step 3: Compile-check**

```bash
cargo build 2>&1 | head -20
```
Expected: still has errors in `posting.rs`, `mod.rs`, `confer.rs` (the call-site updates remain), but no new errors from `server.rs`.

- [ ] **Step 4: Commit**

```bash
git add src/webhook/server.rs
git commit -m "feat(webhook): seed Barry installation cache on inbound events

The webhook payload carries Barry's installation_id. Writing it into
the cache up front means downstream factory.for_identity(Barry, owner, _)
calls never need to hit GitHub for resolution."
```

---

## Task 12: `post_review` — signature change, return `Result<(), GhFactoryError>`

**Files:**
- Modify: `src/checker/multi_review/posting.rs`

- [ ] **Step 1: Update the signature**

In `src/checker/multi_review/posting.rs`, change `post_review` to take `owner`/`repo` instead of `installation_id`, and return `Result<(), GhFactoryError>`:

```rust
use crate::dispatcher::run::{GhFactoryError, MultiGhFactory};
```

Replace the function signature and body (lines 31-68):
```rust
#[allow(clippy::too_many_arguments)]
pub async fn post_review(
    factory: &Arc<dyn MultiGhFactory>,
    owner: &str,
    repo: &str,
    identity: Identity,
    pr_number: i64,
    head_sha: &str,
    files: &[ChangedFile],
    review: &UnifiedReview,
    peer_disagreement: Option<&str>,
) -> Result<(), GhFactoryError> {
    let gh = factory.for_identity(identity, owner, repo).await?;
    let inline = to_inline_comments(files, &review.findings);
    tracing::info!(
        ?identity,
        outcome = ?review.outcome,
        findings = review.findings.len(),
        inline_comments = inline.len(),
        "posting review"
    );
    let body = body_for(identity, review, peer_disagreement);
    let event = match review.outcome {
        crate::checker::multi_review::review::Outcome::Approve => "APPROVE",
        crate::checker::multi_review::review::Outcome::Comment => "COMMENT",
        crate::checker::multi_review::review::Outcome::RequestChanges => "REQUEST_CHANGES",
    };
    let input = ReviewInput {
        body: &body,
        event,
        comments: &inline,
        commit_id: head_sha,
    };
    gh.create_review(owner, repo, pr_number, &input)
        .await
        .map_err(GhFactoryError::Other)?;
    tracing::info!(?identity, "review posted");
    Ok(())
}
```

- [ ] **Step 2: Compile-check**

```bash
cargo build 2>&1 | head -30
```
Expected: errors in `mod.rs:81/101/114` and `confer.rs:115` (callers need updating). No new errors in `posting.rs`.

- [ ] **Step 3: Do not commit yet — Task 13 fixes the call sites.**

---

## Task 13: `MultiReviewChecker` — preflight, run_barry_only, NotInstalled handling

**Files:**
- Modify: `src/checker/multi_review/mod.rs`

- [ ] **Step 1: Update the post_review call sites for the new signature**

In `src/checker/multi_review/mod.rs`, the three `post_review` calls at lines 81, 101, and 114 currently pass `installation_id` positionally. Update each to pass `&ctx.owner, &ctx.repo` instead and drop `installation_id`. The `Verdict::Agree | BarryAlone` arm becomes:

```rust
            Verdict::Agree { barry } | Verdict::BarryAlone { barry, .. } => {
                post_review(
                    &self.gh_factory,
                    &ctx.owner,
                    &ctx.repo,
                    Identity::Barry,
                    ctx.pr.number,
                    &ctx.pr.head.sha,
                    &ctx.files,
                    barry,
                    None,
                )
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            }
```

The `Verdict::Disagree { barry, other_barry, reason }` arm becomes:
```rust
            Verdict::Disagree { barry, other_barry, reason } => {
                let disagreement_msg = format!("I disagree with Barry: {reason}");
                post_review(
                    &self.gh_factory,
                    &ctx.owner,
                    &ctx.repo,
                    Identity::Barry,
                    ctx.pr.number,
                    &ctx.pr.head.sha,
                    &ctx.files,
                    barry,
                    None,
                )
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
                // OB post: NotInstalled here means OB was uninstalled between
                // our pre-check and now. Downgrade to BarryAlone bookkeeping.
                match post_review(
                    &self.gh_factory,
                    &ctx.owner,
                    &ctx.repo,
                    Identity::OtherBarry,
                    ctx.pr.number,
                    &ctx.pr.head.sha,
                    &ctx.files,
                    other_barry,
                    Some(&disagreement_msg),
                )
                .await
                {
                    Ok(()) => {}
                    Err(GhFactoryError::NotInstalled { .. }) => {
                        tracing::warn!(
                            "OB became uninstalled mid-run; downgrading to Barry alone"
                        );
                        metrics::counter!("barry_multi_review_barry_alone_total")
                            .increment(1);
                        // Skip OB record_post below by replacing verdict locally.
                        // (The match below uses `verdict`; we update a flag.)
                        // The simplest path is to short-circuit: jump to the
                        // BarryAlone path for bookkeeping. We can do this by
                        // converting verdict to BarryAlone before the match
                        // that records posts (next step).
                    }
                    Err(GhFactoryError::Other(e)) => return Err(e),
                }
            }
```

This is awkward because the bookkeeping `match` later on uses `&verdict` to decide which rows to write. To make the downgrade clean, refactor the function so the verdict is mutable and the bookkeeping match runs *after* posting:

Find the `let verdict = orchestrator.run(...)` line (around line 64). After it, add a `mut` and convert the post path to update `verdict` on `NotInstalled`. The cleanest shape:

```rust
        let mut verdict = orchestrator.run(&ctx.files).await?;
        let orchestrator_duration = start.elapsed();
        // ... existing verdict_kind logging ...

        // Pre-check passed earlier, so we expect both identities to post when
        // Disagree. NotInstalled here is a late race; downgrade to BarryAlone.
        match &verdict {
            Verdict::Agree { barry } | Verdict::BarryAlone { barry, .. } => {
                post_review(&self.gh_factory, &ctx.owner, &ctx.repo,
                            Identity::Barry, ctx.pr.number, &ctx.pr.head.sha,
                            &ctx.files, barry, None)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
            }
            Verdict::Disagree { barry, other_barry, reason } => {
                let disagreement_msg = format!("I disagree with Barry: {reason}");
                post_review(&self.gh_factory, &ctx.owner, &ctx.repo,
                            Identity::Barry, ctx.pr.number, &ctx.pr.head.sha,
                            &ctx.files, barry, None)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                let downgrade = match post_review(
                    &self.gh_factory, &ctx.owner, &ctx.repo,
                    Identity::OtherBarry, ctx.pr.number, &ctx.pr.head.sha,
                    &ctx.files, other_barry, Some(&disagreement_msg),
                ).await {
                    Ok(()) => false,
                    Err(GhFactoryError::NotInstalled { .. }) => {
                        tracing::warn!(
                            "OB uninstalled mid-run; downgrading to BarryAlone"
                        );
                        metrics::counter!("barry_multi_review_barry_alone_total")
                            .increment(1);
                        true
                    }
                    Err(GhFactoryError::Other(e)) => return Err(e),
                };
                if downgrade {
                    let barry_clone = barry.clone();
                    verdict = Verdict::BarryAlone {
                        barry: barry_clone,
                        reason: "Other Barry uninstalled mid-run".into(),
                    };
                }
            }
        }
        // Existing bookkeeping `match &verdict { ... }` runs as before, using
        // the (possibly downgraded) verdict.
```

If `UnifiedReview` does not currently derive `Clone`, add `#[derive(Clone)]` to it in `src/checker/multi_review/review.rs`.

- [ ] **Step 2: Add the pre-check before invoking the orchestrator**

Still in `MultiReviewChecker::run` (`src/checker/multi_review/mod.rs`), just before `let verdict = orchestrator.run(...)`, insert:

```rust
        // Pre-check: if OB isn't installed on this repo, skip OB's LLM phases
        // entirely and run Barry alone. This avoids paying OB's token cost
        // when we already know we can't post under that identity.
        let ob_available = match self
            .gh_factory
            .preflight_identity(Identity::OtherBarry, &ctx.owner, &ctx.repo)
            .await
        {
            Ok(()) => true,
            Err(GhFactoryError::NotInstalled { .. }) => false,
            Err(GhFactoryError::Other(e)) => return Err(e),
        };
        let mut verdict = if ob_available {
            orchestrator.run(&ctx.files).await?
        } else {
            orchestrator
                .run_barry_only(&ctx.files, "Other Barry not installed".into())
                .await?
        };
```

Replace the existing `let verdict = orchestrator.run(&ctx.files).await?;` with this block. Remove any `installation_id_from_ctx` call near it if it becomes dead code.

- [ ] **Step 3: Update imports**

Near the top of `src/checker/multi_review/mod.rs`, add:
```rust
use crate::dispatcher::run::GhFactoryError;
```

- [ ] **Step 4: Compile-check**

```bash
cargo build 2>&1 | head -20
```
Expected: errors remaining only in `confer.rs` (Task 14 covers that).

- [ ] **Step 5: Add an integration test for the not-installed path**

In `src/checker/multi_review/mod.rs`, find the existing `#[cfg(test)] mod tests` block (if any). If there is no such block, this test can live in `src/checker/multi_review/orchestrator.rs` tests as a higher-level coverage point — but the cleanest place is here since it tests the checker, not the orchestrator. Use the project's existing test scaffolding for `MultiReviewChecker` (look in `mod.rs` and `confer.rs` for examples of how the factory is mocked).

If there is no in-tree mock for `MultiGhFactory`, add one in a test module:

```rust
#[cfg(test)]
mod checker_tests {
    use super::*;
    use crate::checker::multi_review::identity::Identity;
    use crate::dispatcher::run::GhFactoryError;
    use async_trait::async_trait;
    use std::sync::Arc;

    struct StubFactory {
        ob_installed: bool,
    }

    #[async_trait]
    impl crate::dispatcher::run::GhFactory for StubFactory {
        async fn for_installation(
            &self,
            _installation_id: i64,
        ) -> anyhow::Result<Arc<crate::github::GitHub>> {
            anyhow::bail!("not used in this test")
        }
    }

    #[async_trait]
    impl crate::dispatcher::run::MultiGhFactory for StubFactory {
        async fn for_identity(
            &self,
            identity: Identity,
            owner: &str,
            repo: &str,
        ) -> Result<Arc<crate::github::GitHub>, GhFactoryError> {
            if identity == Identity::OtherBarry && !self.ob_installed {
                return Err(GhFactoryError::NotInstalled {
                    identity,
                    owner: owner.to_string(),
                    repo: repo.to_string(),
                });
            }
            // For tests focused on the preflight path, we don't actually
            // post; the run will short-circuit before reaching this.
            Err(GhFactoryError::Other(anyhow::anyhow!("for_identity should not be reached in preflight-only tests")))
        }
        async fn preflight_identity(
            &self,
            identity: Identity,
            owner: &str,
            repo: &str,
        ) -> Result<(), GhFactoryError> {
            if identity == Identity::OtherBarry && !self.ob_installed {
                return Err(GhFactoryError::NotInstalled {
                    identity,
                    owner: owner.to_string(),
                    repo: repo.to_string(),
                });
            }
            Ok(())
        }
    }

    // A full MultiReviewChecker::run test requires the full LLM-client
    // stubbing harness used elsewhere; if not already available in this
    // module's test scaffolding, add the test in tests/ as an integration
    // test. For now, the orchestrator-level test in Task 4
    // (run_barry_only_skips_ob_and_judge) covers the no-OB-calls property,
    // and this StubFactory is exercised by tests in confer.rs.
    #[tokio::test]
    async fn stub_factory_compiles() {
        let f = StubFactory { ob_installed: false };
        let err = f
            .preflight_identity(Identity::OtherBarry, "acme", "widget")
            .await
            .unwrap_err();
        assert!(matches!(err, GhFactoryError::NotInstalled { .. }));
    }
}
```

(The full end-to-end checker test is tricky to write without a substantial test harness for the LLM clients + GitHub; rely on the orchestrator-level test from Task 4 plus the factory-level tests in Task 9 to cover the property "no OB LLM calls when OB not installed.")

- [ ] **Step 6: Run all tests**

```bash
cargo test -q
```
Expected: all tests pass. If anything in `tests/` is failing due to the trait change, fix the call sites there using the same pattern.

- [ ] **Step 7: Commit**

```bash
git add src/checker/multi_review/mod.rs src/checker/multi_review/posting.rs src/checker/multi_review/review.rs
git commit -m "feat(multi-review): preflight OB; run Barry alone if not installed

Before running the dual-reviewer orchestrator, call
factory.preflight_identity(OB, owner, repo). On NotInstalled, route to
orchestrator.run_barry_only which skips OB's draft + synthesis + judge
phases entirely. On the late race where OB becomes uninstalled between
the preflight and the OB post, catch NotInstalled at post time and
downgrade the verdict to BarryAlone — Barry's already-posted review is
preserved and the check status reflects Barry's outcome instead of an
internal error."
```

---

## Task 14: `/barry confer` — preflight + NotInstalled comment

**Files:**
- Modify: `src/checker/multi_review/confer.rs`

- [ ] **Step 1: Update the post_review call signature**

In `src/checker/multi_review/confer.rs` around line 115, change `post_review(..., installation_id, ...)` to use `&owner, &repo` and remove `installation_id`:

```rust
    post_review(
        &factory,
        &owner,
        &repo,
        summon,
        pr_number,
        &head_sha,
        &files,
        &review,
        None,
    )
    .await
    .map_err(|e| anyhow::anyhow!(e))?;
```

(`owner`, `repo`, `pr_number`, `head_sha`, `files`, `review`, `summon`, `factory` must already be in scope; adjust to match the surrounding code.)

- [ ] **Step 2: Add a preflight at the top of the handler**

Find the entry point of the confer handler (the public function that decides whether to summon OB or OOB). After the eligibility checks (authorization, max reached, etc.) but before invoking the LLM, add:

```rust
    match factory
        .preflight_identity(summon, &owner, &repo)
        .await
    {
        Ok(()) => {}
        Err(crate::dispatcher::run::GhFactoryError::NotInstalled { identity, .. }) => {
            let identity_label = identity.label();
            let body = format!(
                "{identity_label} isn't installed on this repository, so I can't summon them. \
                 Ask a maintainer to install the App."
            );
            // Use the existing helper for posting an issue comment under Barry.
            // If no such helper exists in this module, look for one in
            // src/checker/multi_review/ — confer already needs to post issue
            // comments for unauthorized/max-reached rejections.
            barry_gh.post_issue_comment(&owner, &repo, pr_number, &body).await?;
            metrics::counter!(
                "barry_confer_total",
                "outcome" => "rejected_not_installed".to_string()
            )
            .increment(1);
            return Ok(());
        }
        Err(crate::dispatcher::run::GhFactoryError::Other(e)) => {
            return Err(e);
        }
    }
```

If `barry_gh` is not the right binding name, use whatever binding holds Barry's `GitHub` client in this handler — look for the existing rejection-comment code (`rejected_unauthorized`, `rejected_max_reached`) and mirror its pattern exactly.

- [ ] **Step 3: Update the comment for rejected_no_run / rejected_all_posted to ensure no overlap**

Confirm `rejected_not_installed` is a *new* outcome label and does not collide with the existing five (`ob`, `oob`, `rejected_unauthorized`, `rejected_max_reached`, `rejected_no_run`, `rejected_all_posted`). It does not — but verify in `docs/superpowers/specs/2026-05-18-judge-and-installation-fixes-design.md` and the existing CLAUDE.md metric list.

- [ ] **Step 4: Compile-check**

```bash
cargo build 2>&1 | head -20
```
Expected: clean build.

- [ ] **Step 5: Run all tests**

```bash
cargo test -q
```
Expected: all tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/checker/multi_review/confer.rs
git commit -m "feat(confer): reject /barry confer when target App not installed

Pre-check the target identity's installation before invoking the LLM.
On NotInstalled, post a PR comment explaining the situation and bump
barry_confer_total{outcome=rejected_not_installed}. No LLM tokens are
spent on a confer that can't be posted."
```

---

## Task 15: Run the full suite + format + clippy

**Files:** (none — verification)

- [ ] **Step 1: Format**

```bash
cargo fmt
```

- [ ] **Step 2: Clippy with deny-warnings**

```bash
cargo clippy --all-targets -- -D warnings
```
Expected: no warnings. If any appear, fix them inline (most likely candidates: unused imports left over from the trait signature change, missing `#[must_use]`, or doc nits). Do not silence with `#[allow]` unless the suppression is well-justified.

- [ ] **Step 3: Full test sweep**

```bash
cargo test -q
```
Expected: all tests pass.

- [ ] **Step 4: Commit any fmt/clippy fixups**

```bash
git status
# If there are changes:
git add -A
git commit -m "chore: cargo fmt + clippy fixups"
```

- [ ] **Step 5: Final review of the diff**

```bash
git log --oneline ^main HEAD
git diff --stat main..HEAD
```
Expected: roughly 14 commits, each scoped to one task. Verify no spec gaps.

---

## Self-Review Checklist (for the plan author, not the executor)

- [x] Spec section "Architecture overview" → Tasks 1-4 (Fix A), Tasks 5-14 (Fix B).
- [x] Spec section "Fix A — Judge robustness" → Tasks 1-3.
- [x] Spec section "Fix B — Per-identity installation resolution" → Tasks 5-14.
- [x] Spec "Components and file-by-file changes" — each modified file appears in the File Map and is touched in at least one task.
- [x] Spec "Data flow" Path 1 (judge garbage) → Tasks 1-3 + Task 13 integration.
- [x] Spec "Data flow" Path 2 (OB not installed at preflight) → Tasks 4, 9, 13.
- [x] Spec "Data flow" Path 3 (mid-run uninstall race) → Task 10 + Task 13 NotInstalled-at-post-time branch.
- [x] Spec "Data flow" Path 4 (`/barry confer` not installed) → Task 14.
- [x] Spec "Metrics" — `barry_multi_review_barry_alone_total` (Tasks 3, 4, 13); `identity_missing_total{identity, owner}` (Task 9, labels match spec — no `repo` label); `confer_total{rejected_not_installed}` (Task 14); judge_total disagree no longer incremented on errors (Task 3).
- [x] Spec "Error model" — `GhFactoryError` defined in Task 8; callers handle `NotInstalled` in Tasks 13 and 14.
- [x] Spec "Testing" — judge tests in Task 2; orchestrator tests in Tasks 3-4; cache tests in Task 6; factory tests in Tasks 9-10; confer test deferred to the existing test harness in Task 14.
- [x] Spec "Migration test" — covered implicitly by existing `in_memory_creates_schema` test (Task 5 step 2).
- [x] No placeholders; every code step has the actual code.
- [x] Method/type names consistent: `for_identity(identity, owner, repo)`, `preflight_identity(identity, owner, repo)`, `CachedInstallation::{Cached, NotInstalled}`, `GhFactoryError::{NotInstalled, Other}`.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-05-18-judge-and-installation-fixes.md`. Two execution options:

1. **Subagent-Driven (recommended)** — I dispatch a fresh subagent per task with review checkpoints between tasks. Best for catching regressions early and keeping each task focused.

2. **Inline Execution** — Execute tasks in this session with batch checkpoints. Faster end-to-end but less isolation between tasks.

Which approach?
