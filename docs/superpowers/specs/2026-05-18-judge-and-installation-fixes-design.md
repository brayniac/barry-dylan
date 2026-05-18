# Multi-review judge robustness + per-identity installation resolution

## Background

A real production run on `brayniac/levinson` PR #2 surfaced two bugs in the multi-review pipeline that together produced a confusing, wrong-looking check status:

1. The judge LLM returned an empty/unparseable response. The orchestrator's `Err(_)` arm treats *any* judge failure as `Verdict::Disagree`, which forces both Barry and Other Barry to post — even though we have no evidence the two reviewers actually disagreed.
2. The disagreement path then called `factory.for_identity(OtherBarry, installation_id)`, passing Barry's webhook `installation_id` to Other Barry's GitHub App. GitHub installation IDs are scoped per App, so GitHub returned 404 on `POST /app/installations/132083534/access_tokens`. The error propagated out of the checker, and the dispatcher's catch-all converted the in-flight check to `neutral` with summary "internal error (see logs)" — even though Barry's review had already been posted as `RequestChanges`.

Net effect on that PR: Barry's RequestChanges review appeared, the check went neutral instead of failure, and Other Barry never posted anything. Operator-confusing.

This spec covers fixes for both, plus a structural improvement (pre-check installation before running OB's LLM at all) and a one-time WARN+metric so misconfigured installs are visible.

## Goals

- A judge LLM that returns garbage once does not force a noisy double-post.
- Identity → installation_id resolution is correct for OB and OOB, not just Barry.
- When OB (or OOB) is configured but not installed on a repo, the system degrades gracefully: Barry-only review for the standard path, explicit rejection for `/barry confer`.
- Misconfiguration is visible exactly once per `(identity, owner)`, not on every PR.

## Non-goals

- Auto-installing OB/OOB on repos. That is an operator action.
- Changing the judge prompt or judge model selection. Robustness here is about handling the existing judge failing, not making it fail less.
- Cross-org installation discovery (`GET /app/installations` enumeration). We resolve lazily per `(identity, owner)`.
- Per-`(owner, repo)` granularity. GitHub App installations are owner-scoped; caching by owner is sufficient and avoids redundant API calls when Barry reviews many repos in the same org.

## Architecture overview

Two fixes, one PR, shared theme: be honest about uncertainty.

### Fix A — Judge robustness

- `judge::judge` retries exactly once on `JudgeError::Parse` (parse-class failures only; transport errors propagate immediately on the first attempt).
- On every `Parse` failure, log WARN with the raw `resp.text` truncated to 2KB, plus the existing tracing-span context (`pr`, `owner`, `repo`).
- The orchestrator's `Err(_)` arm changes its fallback from `Verdict::Disagree` → `Verdict::BarryAlone { barry: barry_r2, reason: "judge unavailable".into() }`. Metric counted is `barry_multi_review_barry_alone_total`, not `barry_multi_review_judge_total{verdict="disagree"}`.

### Fix B — Per-identity installation resolution

Per-`(identity, owner)` installation cache, resolved lazily and pre-checked before any OB LLM call:

```
factory.for_identity(identity, owner, repo)
  ├─ identity == Barry:
  │     look up cached installation_id for (Barry, owner); resolve via API if absent
  └─ identity == OtherBarry | OtherOtherBarry:
        installation_cache.get(identity, owner):
          HIT  Some(id) → mint token via existing get_or_mint_for(id) path
          HIT  None     → return Err(GhFactoryError::NotInstalled)        [negative cache]
          MISS          → GET /repos/{owner}/{repo}/installation with App JWT
                          200: cache (identity, owner, Some(id)); mint token
                          404: cache (identity, owner, None); WARN + metric;
                               return Err(GhFactoryError::NotInstalled)
```

A sibling `preflight_identity(identity, owner, repo)` returns `Ok(()) | Err(NotInstalled)` without minting a token. The checker calls this *before* running OB's LLM phases, so on a not-installed repo we save the OB token cost entirely and take the existing `BarryAlone` orchestrator path.

## Components and file-by-file changes

### `src/checker/multi_review/judge.rs`

- Extract the single LLM-call-and-parse into an inner helper (e.g., `judge_once`).
- Public `judge(...)` calls `judge_once`; on `Err(JudgeError::Parse(_))`, retries `judge_once` once. Other errors return immediately.
- On *each* `Parse` failure, log WARN with raw `resp.text` truncated to 2KB.

### `src/checker/multi_review/orchestrator.rs`

- Lines 203-213: change the `Err(_)` arm:
  - Log WARN as today, but the message becomes "judge failed; posting Barry alone".
  - Increment `barry_multi_review_barry_alone_total` (not `…judge_total{verdict="disagree"}`).
  - Return `Verdict::BarryAlone { barry: barry_r2, reason: "judge unavailable".into() }`.
- Add `run_barry_only(&files) -> anyhow::Result<Verdict>`:
  - Runs Barry's drafts + R1 synthesis only. Returns `Verdict::BarryAlone { barry: barry_r1, reason: "Other Barry not installed".into() }`.
  - Implementation extracts the existing Barry-only branches at lines 87-92 and 122-132 into a shared helper.

### `src/dispatcher/run.rs`

- `MultiGhFactory` trait:
  - Change `for_identity(identity, installation_id) -> Arc<GitHub>` to `for_identity(identity, owner, repo) -> Result<Arc<GitHub>, GhFactoryError>`.
  - Add `preflight_identity(identity, owner, repo) -> Result<(), GhFactoryError>`.
- New error: `enum GhFactoryError { NotInstalled { identity, owner, repo }, Other(anyhow::Error) }`. `Other` covers transport/auth failures other than 404; callers distinguish.
- `for_installation(installation_id)` stays unchanged (Barry-only legacy path used by code that already has the webhook installation_id in hand).

### `src/app_runtime.rs` (`AppGhFactory`)

- New `for_identity(identity, owner, repo)`:
  - **Barry**: look up `(Barry, owner)` in the installation cache. On hit, mint token via existing `get_or_mint_for`. On miss, resolve via `GET /repos/{owner}/{repo}/installation` with Barry's JWT, write positive cache entry, mint. Note: in normal operation the webhook handler will have populated this cache already (see below).
  - **OB / OOB**: same resolution flow against the respective App's JWT. On 404, write negative cache entry, emit one-time WARN + counter increment, return `Err(NotInstalled)`.
  - **401 on token-mint** (positive cache hit but installation was removed): invalidate the positive cache row for `(identity, owner)`, re-call `for_identity` once. The retry re-queries GitHub and either succeeds (new id) or returns `NotInstalled`. Bounded recursion via an internal flag to prevent loops.
- New `preflight_identity(identity, owner, repo)`: thin wrapper over the same resolution path that returns `Ok(())` instead of minting a token. Same cache writes, same WARN+metric on 404.

### `src/storage/installation_cache.rs` (new) + schema migration

```sql
CREATE TABLE installation_cache (
  identity        TEXT NOT NULL,           -- 'barry' | 'other_barry' | 'other_other_barry'
  owner           TEXT NOT NULL,
  installation_id INTEGER,                 -- NULL = negative cache
  cached_at       INTEGER NOT NULL,        -- unix seconds
  PRIMARY KEY (identity, owner)
) STRICT;
```

Store-actor operations:
- `get_installation(identity, owner, now) -> Option<CacheEntry>` where `CacheEntry = Cached(i64) | NotInstalled`. Negative entries older than 1h are returned as `None` (treat as miss).
- `put_installation(identity, owner, Option<installation_id>, now)`: upsert by primary key.
- `invalidate_installation(identity, owner)`: delete the row (used on 401 token-mint).

Positive entries have no TTL — installation IDs are stable for the life of an installation, and 401 invalidates them when the installation is removed.

### `src/checker/multi_review/posting.rs`

- `post_review` takes `owner: &str, repo: &str` (already in scope via `ctx`) instead of `installation_id`.
- Returns `Result<(), GhFactoryError>` so callers can match on `NotInstalled` directly without downcasting. The single line that today does `let _ = gh.create_review(...)?` continues to use `?` — the `create_review` HTTP error path maps into `GhFactoryError::Other(_)`.

### `src/checker/multi_review/mod.rs`

- Before invoking the orchestrator (line 64 area): call `factory.preflight_identity(OtherBarry, owner, repo)`.
  - `Ok(())` → existing dual-reviewer orchestrator path.
  - `Err(NotInstalled)` → call `orchestrator.run_barry_only(&files)`. Returns `Verdict::BarryAlone`. No OB LLM calls, no judge call.
  - `Err(Other(e))` → propagate (it's a real failure).
- Disagreement path (lines 95-127): wrap the `post_review(OtherBarry, ...)` call. On `NotInstalled` (the late-race case where pre-check passed but install was removed before posting):
  - Skip the OB post.
  - Record only Barry's row in `multi_review` storage (do not call `record_post(OtherBarry, ...)`).
  - Increment `barry_multi_review_barry_alone_total`.
  - The check-run status is derived from Barry's outcome (per existing `verdict.check_outcome()` logic, which we adjust to use the downgraded verdict).
  - Do not propagate the error.
- Other `post_review` errors (`GhFactoryError::Other` — transport, rate limits, etc.) continue to propagate as today.
- After this refactor, `mod.rs` no longer needs `installation_id_from_ctx` for posting (only the factory does, and only internally for Barry's path). The helper can be deleted or left in place if other call sites still use it; the spec leaves that to the implementation plan.

### `src/checker/multi_review/confer.rs`

- At the top of the handler: `factory.preflight_identity(target_identity, owner, repo)`.
  - On `NotInstalled`: post an issue comment with text "Other Other Barry isn't installed on this repo." (substitute identity label). Increment `barry_confer_total{outcome="rejected_not_installed"}`. Return without invoking OOB's LLM.
- Existing rejection paths (unauthorized, max reached, etc.) are unchanged.

### `src/webhook/` (small change)

- After webhook auth validates the `installation` field, write `put_installation(Barry, owner, Some(installation_id), now)` so that downstream `for_identity(Barry, owner, _)` calls hit cache. This makes Barry's path effectively free after the webhook.

## Data flow (key paths)

### Path 1 — Judge returns empty text

```
judge::judge: judge_once → Parse("") → retry judge_once → still Parse → return Err
  WARN log on each Parse with raw text (truncated 2KB)
orchestrator Err arm:
  WARN "judge failed; posting Barry alone"
  counter barry_multi_review_barry_alone_total++
  return Verdict::BarryAlone { barry: barry_r2, reason: "judge unavailable" }
mod.rs BarryAlone arm:
  post_review(Barry); record_post(Barry); skip OB
check status: derived from barry_r2.outcome (Failure for RequestChanges)
```

### Path 2 — OB not installed on the repo

```
mod.rs: factory.preflight_identity(OB, owner, repo)
  installation_cache.get(OB, owner) → miss
  GET /repos/{owner}/{repo}/installation (OB JWT) → 404
  installation_cache.put(OB, owner, None, now)
  WARN "other_barry not installed on {owner}/{repo} — posting Barry alone" (one-time)
  counter barry_multi_review_identity_missing_total{identity=other_barry, owner}++
  return Err(NotInstalled)
mod.rs: orchestrator.run_barry_only(&files) → Verdict::BarryAlone
  no OB LLM calls; no judge call
post_review(Barry); record_post(Barry)
```

### Path 3 — OB was installed; uninstalled mid-run

```
preflight_identity → cache HIT positive id=N → Ok(())
orchestrator runs full dual path → Verdict::Disagree
post_review(Barry) → ok
post_review(OB):
  factory.for_identity(OB, owner, repo) → cache HIT positive id=N
  get_or_mint_for(OB, N) → 401 from access_tokens
  invalidate_installation(OB, owner)
  retry for_identity once → MISS → GET → 404 → negative cache → NotInstalled
post handler catches NotInstalled → downgrade to BarryAlone bookkeeping
  skip OB record_post; counter barry_multi_review_barry_alone_total++
check status: derived from barry's outcome; no error propagated
```

### Path 4 — `/barry confer` with OOB not installed

```
confer handler: factory.preflight_identity(OOB, owner, repo) → NotInstalled
  post issue comment "Other Other Barry isn't installed on this repo."
  counter barry_confer_total{outcome="rejected_not_installed"}++
  return
```

### Path 5 — Happy path (Agree)

```
preflight_identity(OB) → Ok(()) (cache hit, no API call after first review on this org)
orchestrator → Verdict::Agree
post_review(Barry); record_post(Barry)
```

## Metrics

New / changed Prometheus counters:

- `barry_multi_review_barry_alone_total` — existing; now also incremented on judge failure and on late-race NotInstalled during posting (in addition to OB draft/R1 failures).
- `barry_multi_review_identity_missing_total{identity, owner}` — new; incremented once per `(identity, owner)` cache miss that resolves to 404 (matches the cache granularity). Subsequent runs within the 1h negative-cache TTL do not increment. Repo is captured in the WARN log, not in the metric label, to keep cardinality bounded.
- `barry_confer_total{outcome="rejected_not_installed"}` — new label value on existing counter.
- `barry_multi_review_judge_total{verdict="disagree"}` — no longer incremented on judge errors; only on genuine `agree=false` verdicts from a successful judge call.

## Error model

New error type in `src/dispatcher/run.rs`:

```rust
#[derive(Debug, thiserror::Error)]
pub enum GhFactoryError {
    #[error("{identity:?} is not installed on {owner}/{repo}")]
    NotInstalled { identity: Identity, owner: String, repo: String },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
```

Callers handle `NotInstalled` explicitly at:
- `MultiReviewChecker::run` pre-check → `BarryAlone`
- `MultiReviewChecker::run` post-disagreement OB post → downgrade to `BarryAlone`
- `confer` handler → user-visible PR comment

All other variants propagate as `anyhow::Error` and the dispatcher continues to convert to neutral status with "internal error".

## Testing

### Judge robustness
- `judge::judge` retries once on `Parse`: scripted client returns `""` then valid JSON; succeeds on second attempt.
- `judge::judge` returns `Parse` after both attempts: scripted client returns `""` twice; assert error returned; assert truncated raw-text appears in tracing output (use a tracing test layer).
- `judge::judge` does not retry on transport errors: scripted client returns transport error; assert single call, error bubbles up.
- Orchestrator integration (extend existing scripted-client tests): when `judge` returns `Err`, verdict is `BarryAlone` with `reason == "judge unavailable"`; `barry_multi_review_barry_alone_total` is incremented and the `disagree` counter is not.

### Installation cache (Store actor unit tests)
- Positive entry round-trip: `put`/`get` returns `Cached(42)`.
- Negative entry round-trip, fresh: returns `NotInstalled`.
- Negative entry, stale (`cached_at < now - 3600`): returns `None`.
- Positive entries never expire: 100k seconds old still returns `Cached`.
- Different identities don't collide on the same owner.
- `invalidate_installation` deletes the row; subsequent `get` returns `None`.

### Factory (`AppGhFactory`)
HTTP mocked (whatever the existing crate is — likely `wiremock` or `mockito`; will follow existing conventions in `src/github/`).
- `Barry` cache hit: no HTTP, returns token.
- `Barry` cache miss → 200: positive cache row written, returns token.
- `OtherBarry` cache miss → 200 with installation 99: positive cache row written, token minted.
- `OtherBarry` cache hit positive → token mint succeeds.
- `OtherBarry` cache miss → 404: negative cache row written, WARN emitted, counter incremented, returns `NotInstalled`.
- `OtherBarry` cache hit negative (fresh): no HTTP, no WARN, no counter increment, returns `NotInstalled`.
- `OtherBarry` cache hit negative (stale, >1h old): re-queries GitHub.
- `OtherBarry` cache hit positive → token mint 401: cache invalidated, re-resolve; if re-resolve 404s, returns `NotInstalled`; if re-resolve succeeds with new id, returns token. Retry is bounded to one (no infinite loop on permanent 401s).
- `preflight_identity` exercises the same paths but returns `Ok(())` / `Err(NotInstalled)` and does not call the access_tokens endpoint.

### Checker integration
- `MultiReviewChecker::run`, OB not installed at pre-check: orchestrator's `run_barry_only` is called; OB scripted client is never called (verify via call recorder); verdict is `BarryAlone`; only Barry's `record_post` row exists; check status reflects Barry's outcome.
- `MultiReviewChecker::run`, OB installed at pre-check but post-time 401 race: Barry posts, OB post returns `NotInstalled` via the retry path, in-flight verdict is downgraded to `BarryAlone` bookkeeping; OB row not written; no error propagates.
- `MultiReviewChecker::run`, full disagree path with everything installed: unchanged behavior.

### Confer
- `/barry confer` with OOB not installed: factory returns `NotInstalled`; issue comment posted with the documented text; `barry_confer_total{outcome="rejected_not_installed"}` incremented; no OOB LLM calls; no review posted.
- `/barry confer` with OB not installed (when `confer` was about to post OB): same pattern, with the OB-specific message.

### Migration
- `installation_cache` table is created idempotently on startup. Re-running migrations on an existing DB does not error.

## Configuration / operator notes

No new config keys. The fix is transparent to existing `barry.toml`. Operators who see `barry_multi_review_identity_missing_total` increment on a new repo should install the missing App on that org or remove its `[github.other_*]` config block if intentional.

## Open questions

None blocking. Two minor judgment calls have defaults that can be flipped later if needed:
- Positive cache TTL: indefinite (relying on 401-invalidation). If we ever see stale-cache bugs we can add a coarse refresh (e.g., 7d).
- Negative cache TTL: 1h. Short enough that new installs propagate quickly; long enough to suppress per-PR noise.
