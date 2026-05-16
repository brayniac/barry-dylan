# Periodic Status Reporting Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Emit a 60-second status ticker showing the active LLM phase and cumulative token counts per in-flight job; fix the noisy default log filter from `barry_dylan=debug` to `info`.

**Architecture:** New `StatusTracker` (`Arc<RwLock<HashMap<i64, JobProgress>>>`) in `src/telemetry/status.rs`. Workers call `begin`/`complete` when leasing/finishing jobs. The orchestrator calls `set_phase` and `add_tokens` at each LLM phase boundary. A background Tokio task wakes every 60 s and logs a one-line snapshot per active job, or "Barry: idle". Token counts propagate up the call stack via return values from `run_persona`, `synthesize`, and `judge`; the orchestrator accumulates them.

**Tech Stack:** Rust, Tokio (`tokio::time::interval`), `tracing`, `std::sync::RwLock`

---

## File Map

| File | Action | Responsibility |
|---|---|---|
| `src/telemetry/mod.rs` | Modify | Fix log level; add `pub mod status`; add `spawn_status_ticker` |
| `src/telemetry/status.rs` | **Create** | `StatusTracker`, `JobProgress` |
| `src/checker/multi_review/synthesis.rs` | Modify | Add `TokenCount`; add `tokens` to `PersonaDraft`; `synthesize` returns `(UnifiedReview, TokenCount)` |
| `src/checker/multi_review/judge.rs` | Modify | Add `tokens: TokenCount` to `JudgeVerdict`; populate from `LlmResponse` |
| `src/checker/multi_review/confer.rs` | Modify | Fix `run_unified` to destructure `synthesize` tuple |
| `src/checker/multi_review/orchestrator.rs` | Modify | Add `tracker`/`job_id` fields; `synthesize_for` returns tuple; `run()` calls `set_phase`/`add_tokens` |
| `src/checker/multi_review/mod.rs` | Modify | Add `status_tracker` to `MultiReviewChecker`; pass to `Orchestrator` |
| `src/checker/mod.rs` | Modify | Add `pub job_id: i64` to `CheckerCtx` |
| `src/dispatcher/run.rs` | Modify | Add `status_tracker` to `JobDeps`; populate `job_id` in `CheckerCtx` |
| `src/dispatcher/worker.rs` | Modify | Call `begin`/`complete` around job execution |
| `src/app_runtime.rs` | Modify | Create `StatusTracker`, call `spawn_status_ticker`, wire into `JobDeps` and pipeline |

---

### Task 1: Fix default log level

**Files:**
- Modify: `src/telemetry/mod.rs:6`

- [ ] **Step 1: Change the EnvFilter default**

In `src/telemetry/mod.rs` line 6, change:
```rust
        .unwrap_or_else(|_| EnvFilter::new("info,barry_dylan=debug"));
```
to:
```rust
        .unwrap_or_else(|_| EnvFilter::new("info"));
```

- [ ] **Step 2: Verify tests pass**

```bash
cargo test -q
```
Expected: all existing tests pass, no compile errors.

- [ ] **Step 3: Commit**

```bash
git add src/telemetry/mod.rs
git commit -m "fix: lower default log level from debug to info"
```

---

### Task 2: Add StatusTracker and spawn_status_ticker

**Files:**
- Create: `src/telemetry/status.rs`
- Modify: `src/telemetry/mod.rs`

- [ ] **Step 1: Write failing tests in the new file**

Create `src/telemetry/status.rs` containing only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_creates_entry_with_correct_metadata() {
        let t = StatusTracker::new();
        t.begin(1, "acme", "my-repo", 42);
        let snap = t.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].owner, "acme");
        assert_eq!(snap[0].repo, "my-repo");
        assert_eq!(snap[0].pr_number, 42);
        assert_eq!(snap[0].phase, "starting");
        assert_eq!(snap[0].tokens_in, 0);
        assert_eq!(snap[0].tokens_out, 0);
    }

    #[test]
    fn set_phase_updates_name() {
        let t = StatusTracker::new();
        t.begin(1, "o", "r", 1);
        t.set_phase(1, "R1 synthesis");
        let snap = t.snapshot();
        assert_eq!(snap[0].phase, "R1 synthesis");
    }

    #[test]
    fn add_tokens_accumulates_across_calls() {
        let t = StatusTracker::new();
        t.begin(1, "o", "r", 1);
        t.add_tokens(1, 100, 50);
        t.add_tokens(1, 200, 75);
        let snap = t.snapshot();
        assert_eq!(snap[0].tokens_in, 300);
        assert_eq!(snap[0].tokens_out, 125);
    }

    #[test]
    fn complete_removes_entry() {
        let t = StatusTracker::new();
        t.begin(1, "o", "r", 1);
        t.complete(1);
        assert!(t.snapshot().is_empty());
    }

    #[test]
    fn unknown_job_id_is_silently_ignored() {
        let t = StatusTracker::new();
        t.set_phase(999, "whatever");
        t.add_tokens(999, 1, 1);
        t.complete(999);
        // no panic
    }

    #[test]
    fn snapshot_returns_all_active_jobs() {
        let t = StatusTracker::new();
        t.begin(1, "o", "r", 1);
        t.begin(2, "o", "r", 2);
        assert_eq!(t.snapshot().len(), 2);
    }
}
```

- [ ] **Step 2: Verify the tests fail to compile**

```bash
cargo test -q 2>&1 | head -5
```
Expected: compile error referencing `StatusTracker` not found.

- [ ] **Step 3: Implement StatusTracker above the tests**

Prepend this to `src/telemetry/status.rs` before the `#[cfg(test)]` block:

```rust
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct JobProgress {
    pub owner: String,
    pub repo: String,
    pub pr_number: i64,
    pub phase: String,
    pub job_started: Instant,
    pub phase_started: Instant,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

#[derive(Clone, Default)]
pub struct StatusTracker {
    jobs: Arc<RwLock<HashMap<i64, JobProgress>>>,
}

impl StatusTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin(&self, job_id: i64, owner: &str, repo: &str, pr_number: i64) {
        let now = Instant::now();
        let mut jobs = self.jobs.write().unwrap();
        jobs.insert(
            job_id,
            JobProgress {
                owner: owner.to_string(),
                repo: repo.to_string(),
                pr_number,
                phase: "starting".to_string(),
                job_started: now,
                phase_started: now,
                tokens_in: 0,
                tokens_out: 0,
            },
        );
    }

    pub fn set_phase(&self, job_id: i64, phase: &str) {
        let mut jobs = self.jobs.write().unwrap();
        if let Some(p) = jobs.get_mut(&job_id) {
            p.phase = phase.to_string();
            p.phase_started = Instant::now();
        }
    }

    pub fn add_tokens(&self, job_id: i64, input: u64, output: u64) {
        let mut jobs = self.jobs.write().unwrap();
        if let Some(p) = jobs.get_mut(&job_id) {
            p.tokens_in += input;
            p.tokens_out += output;
        }
    }

    pub fn complete(&self, job_id: i64) {
        let mut jobs = self.jobs.write().unwrap();
        jobs.remove(&job_id);
    }

    pub fn snapshot(&self) -> Vec<JobProgress> {
        let jobs = self.jobs.read().unwrap();
        jobs.values().cloned().collect()
    }
}
```

- [ ] **Step 4: Run the tests**

```bash
cargo test telemetry::status -q
```
Expected: 6 tests pass.

- [ ] **Step 5: Wire into telemetry/mod.rs**

Replace the full contents of `src/telemetry/mod.rs` with:

```rust
pub mod status;

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::sync::Arc;
use tracing_subscriber::{EnvFilter, prelude::*};

pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    // Human-readable text to stderr for operators reading in the terminal.
    let text_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    // Structured JSON to stdout for log aggregation / monitoring.
    let json_layer = tracing_subscriber::fmt::layer().json();

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(text_layer)
        .with(json_layer)
        .try_init();
}

pub fn install_metrics() -> PrometheusHandle {
    PrometheusBuilder::new()
        .install_recorder()
        .expect("install Prometheus recorder")
}

pub fn spawn_status_ticker(tracker: Arc<status::StatusTracker>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            let jobs = tracker.snapshot();
            if jobs.is_empty() {
                tracing::info!("Barry: idle");
            } else {
                for job in &jobs {
                    tracing::info!(
                        owner = %job.owner,
                        repo = %job.repo,
                        pr = job.pr_number,
                        phase = %job.phase,
                        elapsed_secs = job.job_started.elapsed().as_secs(),
                        phase_secs = job.phase_started.elapsed().as_secs(),
                        tokens_in = job.tokens_in,
                        tokens_out = job.tokens_out,
                        "Barry: active",
                    );
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn install_metrics_returns_handle() {
        let h = install_metrics();
        let _ = h.render();
    }
}
```

- [ ] **Step 6: Full test run**

```bash
cargo test -q
```
Expected: all tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/telemetry/status.rs src/telemetry/mod.rs
git commit -m "feat: add StatusTracker and 60s status ticker"
```

---

### Task 3: TokenCount type; thread token counts through synthesis, judge, confer, and orchestrator

This task modifies four files atomically — the return-type changes in synthesis and judge must be consumed in orchestrator and confer before the code compiles again.

**Files:**
- Modify: `src/checker/multi_review/synthesis.rs`
- Modify: `src/checker/multi_review/judge.rs`
- Modify: `src/checker/multi_review/confer.rs`
- Modify: `src/checker/multi_review/orchestrator.rs`

- [ ] **Step 1: Add TokenCount + update PersonaDraft in synthesis.rs**

Add `TokenCount` at the top of `src/checker/multi_review/synthesis.rs`, after the existing `use` lines:

```rust
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenCount {
    pub input: u64,
    pub output: u64,
}
```

Update the `PersonaDraft` struct (replace existing definition):

```rust
pub struct PersonaDraft {
    pub persona: &'static str,
    pub raw: String,
    pub tokens: TokenCount,
}
```

Update `run_persona` to populate `tokens` (replace the `Ok(PersonaDraft { ... })` return):

```rust
    let resp = client.complete(&req).await?;
    Ok(PersonaDraft {
        persona: persona.name,
        raw: resp.text,
        tokens: TokenCount {
            input: resp.input_tokens.unwrap_or(0),
            output: resp.output_tokens.unwrap_or(0),
        },
    })
```

Update `synthesize` signature and return type (replace the full function):

```rust
pub async fn synthesize(
    client: &dyn LlmClient,
    drafts: &[PersonaDraft],
    diff_block: &str,
    prior_peer_review: Option<&str>,
    max_tokens: u32,
) -> Result<(UnifiedReview, TokenCount), SynthesisError> {
    let mut user = String::from(SYNTHESIS_TEMPLATE);
    user.push_str("\n\n=== persona drafts ===\n");
    for d in drafts {
        user.push_str(&format!("--- {} ---\n{}\n", d.persona, d.raw));
    }
    if let Some(peer) = prior_peer_review {
        user.push_str("\n=== peer review (R1 from the other Barry) ===\n");
        user.push_str(peer);
        user.push_str("\nYou MAY revise your position based on the peer review. If you do, say so in summary.\n");
    }
    let req = LlmRequest {
        system: Some(diff_block.to_string()),
        messages: vec![LlmMessage {
            role: Role::User,
            content: user,
        }],
        max_tokens,
        temperature: 0.0,
    };
    let resp = client.complete(&req).await?;
    let tokens = TokenCount {
        input: resp.input_tokens.unwrap_or(0),
        output: resp.output_tokens.unwrap_or(0),
    };
    Ok((parse(&resp.text)?, tokens))
}
```

Update the synthesis **tests** that construct `PersonaDraft` directly — add `tokens: TokenCount::default()` to each literal, and destructure the `synthesize` return value:

In `synthesize_includes_all_persona_drafts_in_user`, replace the `PersonaDraft` literals and the `synthesize` call:
```rust
        let drafts = vec![
            PersonaDraft {
                persona: "security",
                raw: "sec-draft".into(),
                tokens: TokenCount::default(),
            },
            PersonaDraft {
                persona: "style",
                raw: "style-draft".into(),
                tokens: TokenCount::default(),
            },
        ];
        let (r, _) = synthesize(&client, &drafts, "DIFF-BLOCK", None, 1024)
            .await
            .unwrap();
        assert_eq!(
            r.outcome,
            crate::checker::multi_review::review::Outcome::Approve
        );
```

In `synthesize_includes_peer_when_provided`, replace the `PersonaDraft` literal and the `synthesize` call:
```rust
        let drafts = vec![PersonaDraft {
            persona: "security",
            raw: "sd".into(),
            tokens: TokenCount::default(),
        }];
        let _ = synthesize(&client, &drafts, "diff", Some("PEER-R1"), 1024)
            .await
            .unwrap();
```

- [ ] **Step 2: Add tokens to JudgeVerdict in judge.rs**

Add this import at the top of `src/checker/multi_review/judge.rs` (after existing `use` lines):

```rust
use crate::checker::multi_review::synthesis::TokenCount;
```

Update `JudgeVerdict` (replace existing definition):

```rust
#[derive(Debug, Clone)]
pub struct JudgeVerdict {
    pub agree: bool,
    pub reason: String,
    pub tokens: TokenCount,
}
```

Update the `Ok(JudgeVerdict { ... })` return in the `judge` function:

```rust
    Ok(JudgeVerdict {
        agree: parsed.agree,
        reason: parsed.reason,
        tokens: TokenCount {
            input: resp.input_tokens.unwrap_or(0),
            output: resp.output_tokens.unwrap_or(0),
        },
    })
```

- [ ] **Step 3: Fix confer.rs run_unified to destructure the tuple**

In `src/checker/multi_review/confer.rs`, in the `run_unified` function, replace the `synthesize` call (lines ~179–182):

```rust
    let (r, _tokens) = synthesis::synthesize(client, &drafts, diff, peer, max_tokens)
        .await
        .map_err(|e| anyhow::anyhow!("synthesis failed: {e}"))?;
    Ok(r)
```

- [ ] **Step 4: Update orchestrator.rs synthesize_for to return a tuple**

In `src/checker/multi_review/orchestrator.rs`, add this import at the top:

```rust
use crate::checker::multi_review::synthesis::TokenCount;
```

Replace the `synthesize_for` method signature and body:

```rust
    async fn synthesize_for(
        &self,
        identity: Identity,
        diff: &str,
        drafts: &[PersonaDraft],
        peer: Option<&str>,
    ) -> anyhow::Result<(UnifiedReview, TokenCount)> {
        let round = if peer.is_some() { "R2" } else { "R1" };
        let client = self.clients.for_identity(identity);
        let max_tokens = self.clients.max_tokens_for(identity);
        let start = std::time::Instant::now();

        let result = synthesis::synthesize(client.as_ref(), drafts, diff, peer, max_tokens)
            .await
            .map_err(|e| anyhow::anyhow!("synthesis failed: {e}"));

        let duration_ms = start.elapsed().as_millis() as u64;
        match result {
            Ok((review, tokens)) => {
                tracing::info!(
                    identity = ?identity,
                    round,
                    duration_ms,
                    outcome = format!("{:?}", review.outcome),
                    "synthesis done"
                );
                Ok((review, tokens))
            }
            Err(e) => Err(e),
        }
    }
```

Update `run()` to destructure all synthesis and judge returns. Replace the body of `run()` with the version below (preserving all existing tracing/metrics calls, just updating variable bindings):

In the OB-drafts-failure early-return arm (currently calls `synthesize_for` for Barry alone):
```rust
        let ob_drafts = match ob_drafts {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(?e, "Other Barry persona drafts failed; Barry posts alone");
                let barry_draft_tokens: u64 = barry_drafts.iter().map(|d| d.tokens.input).sum();
                let barry_draft_tokens_out: u64 = barry_drafts.iter().map(|d| d.tokens.output).sum();
                let (barry_r1, r1_tokens) = self
                    .synthesize_for(Identity::Barry, &diff, &barry_drafts, None)
                    .await
                    .map_err(|e| anyhow::anyhow!("barry R1 failed: {e}"))?;
                let _ = (barry_draft_tokens, barry_draft_tokens_out, r1_tokens); // wired in Task 6
                tracing::info!(kind = "barry_alone", "verdict");
                metrics::counter!("barry_multi_review_barry_alone_total").increment(1);
                return Ok(Verdict::BarryAlone {
                    barry: barry_r1,
                    reason: format!("Other Barry unavailable: {e}"),
                });
            }
        };
        // TASK6: add draft token accumulation here (normal path)
```

Replace the R1 section (after `let drafts_start` block):
```rust
        tracing::debug!("R1 synthesis starting");
        let r1_start = std::time::Instant::now();
        let (barry_r1_res, ob_r1_res) = tokio::join!(
            self.synthesize_for(Identity::Barry, &diff, &barry_drafts, None),
            self.synthesize_for(Identity::OtherBarry, &diff, &ob_drafts, None),
        );
        let (barry_r1, barry_r1_tokens) = match barry_r1_res {
            Ok(t) => t,
            Err(e) => return Err(anyhow::anyhow!("barry R1 failed: {e}")),
        };
        let (ob_r1, ob_r1_tokens) = match ob_r1_res {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(?e, "Other Barry R1 synthesis failed; Barry posts alone");
                tracing::info!(kind = "barry_alone", "verdict");
                metrics::counter!("barry_multi_review_barry_alone_total").increment(1);
                return Ok(Verdict::BarryAlone {
                    barry: barry_r1,
                    reason: format!("Other Barry unavailable: {e}"),
                });
            }
        };
        tracing::info!(
            duration_ms = r1_start.elapsed().as_millis() as u64,
            barry_outcome = ?barry_r1.outcome,
            ob_outcome = ?ob_r1.outcome,
            "R1 synthesis complete"
        );
        let _ = (barry_r1_tokens, ob_r1_tokens); // wired in Task 6
```

Replace the R2 section:
```rust
        tracing::debug!("R2 synthesis starting (drafts reused from R1)");
        let r2_start = std::time::Instant::now();
        let (barry_r2_res, ob_r2_res) = tokio::join!(
            self.synthesize_for(Identity::Barry, &diff, &barry_drafts, Some(&ob_r1_text)),
            self.synthesize_for(
                Identity::OtherBarry,
                &diff,
                &ob_drafts,
                Some(&barry_r1_text)
            ),
        );
        let (barry_r2, barry_r2_tokens) = match barry_r2_res {
            Ok(t) => t,
            Err(_) => (barry_r1, TokenCount::default()),
        };
        let (ob_r2, ob_r2_tokens) = match ob_r2_res {
            Ok(t) => t,
            Err(_) => (ob_r1, TokenCount::default()),
        };
        tracing::info!(
            duration_ms = r2_start.elapsed().as_millis() as u64,
            barry_outcome = ?barry_r2.outcome,
            ob_outcome = ?ob_r2.outcome,
            "R2 synthesis complete"
        );
        let _ = (barry_r2_tokens, ob_r2_tokens); // wired in Task 6
```

The judge section needs no changes to the call itself — `verdict.tokens` is just a new field that isn't read yet. The judge call and verdict returns remain exactly as before.

- [ ] **Step 5: Verify tests pass**

```bash
cargo test -q
```
Expected: all tests pass (no compile errors, all existing orchestrator tests pass).

- [ ] **Step 6: Commit**

```bash
git add src/checker/multi_review/synthesis.rs \
        src/checker/multi_review/judge.rs \
        src/checker/multi_review/confer.rs \
        src/checker/multi_review/orchestrator.rs
git commit -m "feat: add TokenCount; thread token counts through synthesis, judge, and orchestrator"
```

---

### Task 4: Thread StatusTracker + job_id through infrastructure

Adds the new fields to structs in four files; also wires `app_runtime.rs` (must happen together to keep compilation clean).

**Files:**
- Modify: `src/checker/mod.rs`
- Modify: `src/dispatcher/run.rs`
- Modify: `src/checker/multi_review/mod.rs`
- Modify: `src/checker/multi_review/orchestrator.rs`
- Modify: `src/app_runtime.rs`

- [ ] **Step 1: Add job_id to CheckerCtx**

In `src/checker/mod.rs`, add one field to `CheckerCtx`:

```rust
pub struct CheckerCtx {
    pub gh: Arc<GitHub>,
    pub repo_cfg: Arc<RepoConfig>,
    pub owner: String,
    pub repo: String,
    pub pr: Arc<PullRequest>,
    pub files: Vec<ChangedFile>,
    pub prior_bot_reviews: Vec<BotComment>,
    pub prior_bot_comments: Vec<BotComment>,
    pub store: crate::storage::Store,
    pub installation_id: Option<i64>,
    pub job_id: i64,                     // ← new
}
```

- [ ] **Step 2: Add status_tracker to JobDeps and populate job_id in CheckerCtx**

In `src/dispatcher/run.rs`, add the import at the top:

```rust
use crate::telemetry::status::StatusTracker;
```

Add one field to `JobDeps`:

```rust
pub struct JobDeps {
    pub store: Store,
    pub config: Arc<Config>,
    pub pipeline: Arc<Pipeline>,
    pub gh_factory: Arc<dyn MultiGhFactory>,
    pub clients: Option<Arc<crate::checker::multi_review::clients::IdentityClients>>,
    pub personas: Option<Arc<Vec<crate::checker::multi_review::persona::Persona>>>,
    pub status_tracker: Arc<StatusTracker>,    // ← new
}
```

In `run_job`, in the `CheckerCtx` construction block, add `job_id`:

```rust
    let ctx = CheckerCtx {
        gh: gh.clone(),
        repo_cfg: Arc::new(repo_cfg),
        owner: job.repo_owner.clone(),
        repo: job.repo_name.clone(),
        pr: Arc::new(pr),
        files,
        prior_bot_reviews: bot_reviews,
        prior_bot_comments: bot_comments,
        store: deps.store.clone(),
        installation_id: Some(job.installation_id),
        job_id: job.id,                         // ← new
    };
```

- [ ] **Step 3: Add status_tracker to MultiReviewChecker and pass to Orchestrator**

In `src/checker/multi_review/mod.rs`, add the import:

```rust
use crate::telemetry::status::StatusTracker;
```

Add one field to `MultiReviewChecker`:

```rust
pub struct MultiReviewChecker {
    pub clients: Arc<IdentityClients>,
    pub personas: Arc<Vec<Persona>>,
    pub gh_factory: Arc<dyn MultiGhFactory>,
    pub status_tracker: Arc<StatusTracker>,   // ← new
}
```

In `MultiReviewChecker::run`, replace the `Orchestrator` construction:

```rust
        let orchestrator = Orchestrator {
            clients: &self.clients,
            personas: &self.personas,
            tracker: self.status_tracker.clone(),
            job_id: ctx.job_id,
        };
```

- [ ] **Step 4: Add tracker and job_id fields to Orchestrator and update its tests**

In `src/checker/multi_review/orchestrator.rs`, add the import:

```rust
use crate::telemetry::status::StatusTracker;
```

Update the `Orchestrator` struct:

```rust
pub struct Orchestrator<'a> {
    pub clients: &'a IdentityClients,
    pub personas: &'a [Persona],
    pub tracker: Arc<StatusTracker>,
    pub job_id: i64,
}
```

In the `#[cfg(test)]` module, add:

```rust
use crate::telemetry::status::StatusTracker;
```

Update every `Orchestrator { clients: &c, personas: &p }` construction in the tests to include the new fields:

```rust
        Orchestrator {
            clients: &c,
            personas: &p,
            tracker: Arc::new(StatusTracker::new()),
            job_id: 0,
        }
```

There are four test functions (`agreement_returns_agree_with_barry`, `disagreement_returns_both`, `ob_failure_yields_barry_alone`, `judge_failure_defaults_to_disagree`) — update all four.

- [ ] **Step 5: Wire app_runtime.rs**

In `src/app_runtime.rs`, add the import at the top:

```rust
use crate::telemetry::status::StatusTracker;
```

In the `run` function, after `let metrics = crate::telemetry::install_metrics();`, add:

```rust
    let status_tracker = Arc::new(StatusTracker::new());
    crate::telemetry::spawn_status_ticker(status_tracker.clone());
```

Update the `deps` construction to include `status_tracker`:

```rust
    let deps = Arc::new(JobDeps {
        store: store.clone(),
        config: cfg.clone(),
        pipeline: pipeline.clone(),
        gh_factory: gh_factory.clone(),
        clients: Some(clients),
        personas: Some(personas),
        status_tracker: status_tracker.clone(),
    });
```

Update `build_pipeline_with` to accept and thread the tracker (add parameter and pass to `MultiReviewChecker`):

```rust
fn build_pipeline_with(
    clients: Arc<crate::checker::multi_review::clients::IdentityClients>,
    personas: Arc<Vec<crate::checker::multi_review::persona::Persona>>,
    gh_factory: Arc<dyn MultiGhFactory>,
    status_tracker: Arc<StatusTracker>,
) -> Pipeline {
    let mut p = Pipeline::hygiene_only();
    p.checkers
        .push(Arc::new(crate::checker::multi_review::MultiReviewChecker {
            clients,
            personas,
            gh_factory,
            status_tracker,
        }));
    p
}
```

Update the call site for `build_pipeline_with` to pass `status_tracker`:

```rust
    let pipeline = Arc::new(build_pipeline_with(
        clients.clone(),
        personas.clone(),
        gh_factory.clone(),
        status_tracker.clone(),
    ));
```

- [ ] **Step 6: Full build and test**

```bash
cargo test -q
```
Expected: all tests pass. If there are "unused import" warnings, fix them now.

- [ ] **Step 7: Commit**

```bash
git add src/checker/mod.rs \
        src/dispatcher/run.rs \
        src/checker/multi_review/mod.rs \
        src/checker/multi_review/orchestrator.rs \
        src/app_runtime.rs
git commit -m "feat: thread StatusTracker and job_id through infrastructure"
```

---

### Task 5: Wire worker begin/complete

**Files:**
- Modify: `src/dispatcher/worker.rs`

- [ ] **Step 1: Call begin after lease and complete in all exit paths**

In `src/dispatcher/worker.rs`, after the line `let id = leased.id;` (currently line 34), add:

```rust
        deps.status_tracker.begin(id, &leased.repo_owner, &leased.repo_name, leased.pr_number);
```

In the `Ok(())` success path, after `let _ = deps.store.ack(id).await;`, add:

```rust
                deps.status_tracker.complete(id);
```

In the rate-limited path, after the `let _ = deps.store.reschedule_at(...)` call and before `continue;`, add:

```rust
                    deps.status_tracker.complete(id);
```

In the permanent-drop `else` branch (after `nack` determines `alive == false`), add after the `tracing::error!` call:

```rust
                    deps.status_tracker.complete(id);
```

In the retry branch (when `alive == true`), add after the `tracing::warn!` call:

```rust
                    deps.status_tracker.complete(id);
```

- [ ] **Step 2: Verify tests pass**

```bash
cargo test -q
```
Expected: all tests pass.

- [ ] **Step 3: Commit**

```bash
git add src/dispatcher/worker.rs
git commit -m "feat: register jobs in StatusTracker from worker"
```

---

### Task 6: Wire orchestrator set_phase and add_tokens calls

**Files:**
- Modify: `src/checker/multi_review/orchestrator.rs`

- [ ] **Step 1: Replace the placeholder suppression lines with real tracker calls**

In `src/checker/multi_review/orchestrator.rs`, in the `run()` method, make the following changes:

**Before persona drafts** (add after the `tracing::info!("multi-review orchestration starting")` line):

```rust
        self.tracker.set_phase(self.job_id, "persona drafts");
```

**After barry_drafts/ob_drafts unwrap succeed** (replace the `// TASK6: add draft token accumulation here (normal path)` comment):

```rust
        let draft_tok_in: u64 = barry_drafts.iter().chain(ob_drafts.iter()).map(|d| d.tokens.input).sum();
        let draft_tok_out: u64 = barry_drafts.iter().chain(ob_drafts.iter()).map(|d| d.tokens.output).sum();
        self.tracker.add_tokens(self.job_id, draft_tok_in, draft_tok_out);
```

**In the OB-drafts-failure early-return arm**, make two changes:

First, replace `let _ = (barry_draft_tokens, barry_draft_tokens_out, r1_tokens); // wired in Task 6` with:
```rust
                self.tracker.add_tokens(self.job_id, barry_draft_tokens, barry_draft_tokens_out);
                self.tracker.set_phase(self.job_id, "R1 synthesis");
```
(These two lines go _before_ the `synthesize_for` call; you are replacing the suppression line that came after it. Move these two lines to sit between the `barry_draft_tokens_out` computation and the `synthesize_for` call, then remove the suppression.)

Then, directly after the `synthesize_for` call (after the `?` on the `r1_tokens` binding), add:
```rust
                self.tracker.add_tokens(self.job_id, r1_tokens.input, r1_tokens.output);
```

**Before R1 synthesis** (add before `tracing::debug!("R1 synthesis starting")`):

```rust
        self.tracker.set_phase(self.job_id, "R1 synthesis");
```

**After R1 tracing::info!** (replace `let _ = (barry_r1_tokens, ob_r1_tokens);`):

```rust
        self.tracker.add_tokens(
            self.job_id,
            barry_r1_tokens.input + ob_r1_tokens.input,
            barry_r1_tokens.output + ob_r1_tokens.output,
        );
```

**Before R2 synthesis** (add before `tracing::debug!("R2 synthesis starting...")`):

```rust
        self.tracker.set_phase(self.job_id, "R2 synthesis");
```

**After R2 tracing::info!** (replace `let _ = (barry_r2_tokens, ob_r2_tokens);`):

```rust
        self.tracker.add_tokens(
            self.job_id,
            barry_r2_tokens.input + ob_r2_tokens.input,
            barry_r2_tokens.output + ob_r2_tokens.output,
        );
```

**Before judge call** (add before `tracing::debug!("judge starting")`):

```rust
        self.tracker.set_phase(self.job_id, "judge");
```

**After the judge `tracing::info!` call** (add after the `"judge done"` log):

```rust
        self.tracker.add_tokens(self.job_id, verdict.tokens.input, verdict.tokens.output);
```

- [ ] **Step 2: Full test run**

```bash
cargo test -q
```
Expected: all tests pass.

- [ ] **Step 3: Lint check**

```bash
cargo clippy -- -D warnings
```
Fix any warnings before committing.

- [ ] **Step 4: Final commit**

```bash
git add src/checker/multi_review/orchestrator.rs
git commit -m "feat: wire set_phase and add_tokens in orchestrator"
```

---

## Verification

After all tasks complete, run:

```bash
cargo test -q && cargo clippy -- -D warnings && cargo build
```

Expected: clean build, all tests green, no clippy warnings.

Manual verification: start barry with `RUST_LOG=info` (or no override), trigger a PR review, and watch stderr for:
- `"Barry: active"` lines every ~60 s while LLM calls are in flight, showing `phase`, `elapsed_secs`, `tokens_in`, `tokens_out`
- `"Barry: idle"` when the queue drains
- No `DEBUG` lines appearing without explicit `RUST_LOG=debug`
