# Periodic Status Reporting

**Date:** 2026-05-16  
**Status:** Approved

## Problem

The default log filter includes `barry_dylan=debug`, producing noisy output. Long-running LLM operations (persona drafts, R1/R2 synthesis, judge — each potentially 30–90 seconds) emit nothing while in flight, so there is no way to tell whether Barry is working or stuck.

## Goal

Every 60 seconds, emit a single status line per active job showing the current LLM phase, elapsed time, and cumulative token counts. Print "Barry: idle" when nothing is running. Also fix the default log level to `info`.

## Design

### 1. `StatusTracker` (`src/telemetry/status.rs`)

```rust
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

pub struct StatusTracker {
    jobs: Arc<RwLock<HashMap<i64, JobProgress>>>,
}
```

Public API:

| Method | Effect |
|---|---|
| `begin(job_id, owner, repo, pr_number)` | Inserts entry with phase = "starting" |
| `set_phase(job_id, phase: &str)` | Updates phase name, resets `phase_started` |
| `add_tokens(job_id, in: u64, out: u64)` | Accumulates into running totals |
| `complete(job_id)` | Removes entry |
| `snapshot() -> Vec<JobProgress>` | Cheap clone of all active entries |

All methods are infallible — a missing `job_id` is silently ignored (the tracker is advisory).

### 2. Background ticker (`src/telemetry/mod.rs` + `src/app_runtime.rs`)

`telemetry::spawn_status_ticker(tracker: Arc<StatusTracker>)` spawns a Tokio task:

```
every 60s:
  let snap = tracker.snapshot();
  if snap.is_empty():
    tracing::info!("Barry: idle")
  else:
    for job in snap:
      tracing::info!(
        owner, repo, pr = job.pr_number,
        phase = job.phase,
        elapsed_secs = job.job_started.elapsed().as_secs(),
        phase_secs = job.phase_started.elapsed().as_secs(),
        tokens_in = job.tokens_in,
        tokens_out = job.tokens_out,
        "Barry: active"
      )
```

Sample stderr output:
```
Barry: active  owner=brayniac repo=barry-dylan pr=42  phase="R1 synthesis"  elapsed=62s  phase=11s  tok_in=3200  tok_out=840
Barry: idle
```

Called once from `app_runtime.rs` after the `StatusTracker` is created.

### 3. Token propagation

Three synthesis-layer types gain a `tokens: TokenCount` field:

```rust
pub struct TokenCount { pub input: u64, pub output: u64 }
```

- `PersonaDraft` — populated from `LlmResponse.{input,output}_tokens` in `run_persona`
- `synthesize` — returns `(UnifiedReview, TokenCount)` tuple
- `JudgeVerdict` — gains `tokens: TokenCount`

`None` token values in `LlmResponse` map to 0. The orchestrator is the only consumer of these counts; synthesis and judge remain decoupled from the tracker.

### 4. Orchestrator wiring (`src/checker/multi_review/orchestrator.rs`)

`Orchestrator` gains two fields:

```rust
pub tracker: Arc<StatusTracker>,
pub job_id: i64,
```

Phase update sequence inside `run()`:

| Before | Call |
|---|---|
| `tokio::join!(run_persona_drafts…)` | `set_phase(job_id, "persona drafts")` |
| `tokio::join!(synthesize_for R1…)` | `set_phase(job_id, "R1 synthesis")` |
| `tokio::join!(synthesize_for R2…)` | `set_phase(job_id, "R2 synthesis")` |
| `judge::judge(…)` | `set_phase(job_id, "judge")` |

After each phase, `add_tokens` is called with the sum of all `TokenCount`s returned from that phase.

`Orchestrator` is constructed in `run_job` (`dispatcher/run.rs`), which already has access to `JobDeps` (where the tracker lives) and `job.id`.

### 5. Worker wiring (`src/dispatcher/worker.rs`)

- After a job is successfully leased: `deps.status_tracker.begin(id, owner, repo, pr_number)`
- In the `Ok(())` path after `ack`: `deps.status_tracker.complete(id)`
- In the `Err` path after `nack`/`reschedule_at`: `deps.status_tracker.complete(id)`

### 6. `JobDeps` (`src/dispatcher/run.rs`)

```rust
pub struct JobDeps {
    // existing fields …
    pub status_tracker: Arc<StatusTracker>,
}
```

### 7. Log level fix (`src/telemetry/mod.rs`)

```rust
// Before
EnvFilter::new("info,barry_dylan=debug")

// After
EnvFilter::new("info")
```

`RUST_LOG` still overrides at runtime for debugging.

## Files Changed

| File | Change |
|---|---|
| `src/telemetry/status.rs` | New — `StatusTracker`, `JobProgress`, `TokenCount` |
| `src/telemetry/mod.rs` | Add `spawn_status_ticker`; fix default log filter |
| `src/dispatcher/run.rs` | Add `status_tracker` to `JobDeps` |
| `src/dispatcher/worker.rs` | Call `begin`/`complete` around job execution |
| `src/checker/multi_review/synthesis.rs` | Add `tokens` to `PersonaDraft`; `synthesize` returns tuple |
| `src/checker/multi_review/judge.rs` | Add `tokens` to `JudgeVerdict` |
| `src/checker/multi_review/orchestrator.rs` | Add tracker + job_id fields; call `set_phase`/`add_tokens` |
| `src/app_runtime.rs` | Create `StatusTracker`, call `spawn_status_ticker`, pass to `JobDeps` |

## Out of Scope

- Hygiene checker phases (fast, not LLM-backed)
- Confer path (separate flow, low priority)
- Persisting status to SQLite
