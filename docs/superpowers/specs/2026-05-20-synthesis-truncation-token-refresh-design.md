# Synthesis Truncation + Token Refresh Design

## Goal

Fix two production bugs observed in a long-running job (PR #4, levinson repo):

1. **Synthesis truncation**: Both R1 synthesis LLM calls hit `max_tokens=131072`, truncating their output mid-JSON, causing `parse: could not locate JSON object in model output`. Barry posted nothing.
2. **Token expiry on post_outcome**: The job ran ~78 minutes. The installation token minted at job start (valid for 1 hour) had expired by the time `post_outcome` ran, producing `401 Bad credentials`.

## Architecture

Two independent fixes, one spec and plan.

**Fix A — Synthesis truncation:** Surface `finish_reason` from both LLM providers, retry once in `synthesize()` on truncation, fall back to a draft-based review if still truncated. Also lower the default synthesis `max_tokens` so truncation happens fast (less wasted token spend) and the concise prompt constraint improves output quality.

**Fix B — Token refresh:** Re-mint Barry's GitHub client immediately before the outcome posting loop so long-running jobs never present an expired token to the GitHub API.

---

## Fix A: Synthesis Truncation

### A1 — `FinishReason` in `LlmResponse` (`src/llm/mod.rs`)

Add a `FinishReason` enum and a new field on `LlmResponse`:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum FinishReason {
    Stop,
    Length,
    Other(String),
}

pub struct LlmResponse {
    pub text: String,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub finish_reason: Option<FinishReason>,  // new
}
```

### A2 — Capture finish_reason in providers

**`src/llm/anthropic.rs`**: Deserialize `stop_reason` from the response body. Map `"end_turn"` → `Stop`, `"max_tokens"` → `Length`, anything else → `Other(s)`. Populate `LlmResponse::finish_reason`.

**`src/llm/openai.rs`**: Deserialize `finish_reason` from `choices[0]`. Map `"stop"` → `Stop`, `"length"` → `Length`, anything else → `Other(s)`. Populate `LlmResponse::finish_reason`.

### A3 — Log finish_reason in factory (`src/llm/factory.rs`)

The existing `llm call completed` log line gains a `finish_reason` field:

```
finish_reason = %finish_reason  // "stop" | "length" | "other:..."
```

This surfaces in the 60-second status ticker so operators can see truncation in flight.

### A4 — Truncation variant in `SynthesisError` (`src/checker/multi_review/synthesis.rs`)

```rust
pub enum SynthesisError {
    Llm(#[from] LlmError),
    Parse(#[from] ParseError),
    Truncated,  // new: model hit max_tokens on both attempts
}
```

In `synthesize()`, check `finish_reason` before calling `parse()`. If `Length`, log a WARN and retry once at the same budget. If still `Length` on retry, return `SynthesisError::Truncated`.

```rust
// after resp = client.complete(&req).await?:
if matches!(resp.finish_reason, Some(FinishReason::Length)) {
    tracing::warn!(
        input_tokens = resp.input_tokens,
        output_tokens = resp.output_tokens,
        "synthesis truncated at max_tokens; retrying once"
    );
    resp = client.complete(&req).await?;
    if matches!(resp.finish_reason, Some(FinishReason::Length)) {
        tracing::warn!("synthesis truncated on retry; giving up");
        return Err(SynthesisError::Truncated);
    }
}
Ok((parse(&resp.text)?, tokens))
```

### A5 — Fallback in orchestrator (`src/checker/multi_review/orchestrator.rs`)

When `synthesize_for(Barry, ...)` returns `SynthesisError::Truncated`, the orchestrator falls back to a minimal `UnifiedReview` constructed from Barry's already-computed persona drafts, then returns `Verdict::BarryAlone`. No LLM retries, no re-running drafts.

Add a helper in `src/checker/multi_review/review.rs`:

```rust
/// Construct a minimal Comment-outcome review from persona draft text.
/// Used as a fallback when synthesis is truncated.
pub fn from_drafts(drafts: &[super::synthesis::PersonaDraft]) -> UnifiedReview {
    let summary = drafts
        .iter()
        .map(|d| format!("**{}**\n{}", d.persona, d.raw))
        .collect::<Vec<_>>()
        .join("\n\n");
    UnifiedReview {
        outcome: Outcome::Comment,
        summary,
        findings: vec![],
    }
}
```

In `orchestrator.run()`, replace the current Barry R1 error arm:

```rust
let (barry_r1, barry_r1_tokens) = match barry_r1_res {
    Ok(r) => r,
    Err(SynthesisError::Truncated) => {
        tracing::warn!("barry R1 synthesis truncated; falling back to drafts");
        metrics::counter!(
            "barry_multi_review_truncated_total",
            "phase" => "r1_synthesis"
        ).increment(1);
        return Ok(Verdict::BarryAlone {
            barry: review::from_drafts(&barry_drafts),
            reason: "synthesis truncated".into(),
        });
    }
    Err(e) => return Err(anyhow::anyhow!("barry R1 failed: {e}")),
};
```

Apply the same pattern in `run_barry_only()` for its `synthesize_for` call.

### A6 — Lower default synthesis max_tokens

The `max_tokens` value is per-LLM-profile in `barry.toml`. The current user config has `max_tokens = 131072`. The recommended value for synthesis is `32768` — this forces concise structured output and makes truncation failures fast (seconds rather than 18 minutes per call).

No code change required: this is a config update documented here for operator awareness. The `default_max_tokens()` in `src/config/mod.rs` (currently 8192) is already conservative and appropriate.

---

## Fix B: Token Refresh Before post_outcome

### B1 — Re-mint before outcome loop (`src/dispatcher/run.rs`)

In `run_job()`, after all checker task `join_all` calls complete and before the outcome-posting loop, re-acquire Barry's GitHub client:

```rust
// Re-mint Barry's token; long-running jobs may have outlasted the 1-hour expiry.
let gh = deps.gh_factory.for_installation(job.installation_id).await?;
```

This shadows the `gh` from job start. `get_or_mint_for` checks `expires_at - 300` (5-minute buffer): if the token is still valid it returns the cached value immediately; if expired or within the buffer it re-mints. One extra DB read in the common case, one API call in the expired case.

No changes to `post_outcome` signature or `GitHub` client struct.

---

## Error Model

- `SynthesisError::Truncated` → orchestrator → `Verdict::BarryAlone` with draft-based review. Barry posts a Comment-outcome review with raw persona text. Check run reflects Barry's outcome.
- `401` on token re-mint (before post_outcome) → `run_job` returns `Err`, job is retried by the worker lease loop with exponential backoff.

---

## Metrics

- `barry_multi_review_truncated_total{phase="r1_synthesis"}` — incremented when Barry's R1 synthesis falls back to drafts after truncation.
- Existing `barry_multi_review_barry_alone_total` — incremented when `Verdict::BarryAlone` is returned (already covers truncation path since it routes through the same posting code).

---

## Testing

- **`src/llm/anthropic.rs` tests**: Mock response with `"stop_reason": "max_tokens"` → `FinishReason::Length`. Mock with `"end_turn"` → `FinishReason::Stop`.
- **`src/llm/openai.rs` tests**: Mock response with `"finish_reason": "length"` → `FinishReason::Length`. Mock with `"stop"` → `FinishReason::Stop`.
- **`src/checker/multi_review/synthesis.rs` tests**: `ScriptClient` (already exists from judge tests) that returns `finish_reason: Length` for first call, `Stop` for second → verify retry succeeds. Two `Length` responses → verify `SynthesisError::Truncated`. Two `Stop` responses → verify no retry.
- **`src/checker/multi_review/orchestrator.rs` tests**: Mock synthesis client that returns truncated on first call → verify `Verdict::BarryAlone` with `from_drafts` content, metric increment, no OB calls.
- **`src/dispatcher/run.rs`**: Existing tests cover the basic path; no new unit tests needed for the re-mint (it's a one-liner using already-tested machinery). Integration test coverage via the existing `tests/` suite.

---

## Files Changed

| File | Change |
|------|--------|
| `src/llm/mod.rs` | Add `FinishReason` enum; add field to `LlmResponse` |
| `src/llm/anthropic.rs` | Deserialize `stop_reason`; populate `finish_reason` |
| `src/llm/openai.rs` | Deserialize `finish_reason` from choices; populate field |
| `src/llm/factory.rs` | Log `finish_reason` in completion log line |
| `src/checker/multi_review/synthesis.rs` | Add `SynthesisError::Truncated`; truncation check + retry in `synthesize()` |
| `src/checker/multi_review/review.rs` | Add `from_drafts()` helper |
| `src/checker/multi_review/orchestrator.rs` | Handle `Truncated` in Barry R1 arm and `run_barry_only`; emit metric |
| `src/dispatcher/run.rs` | Re-mint `gh` before outcome loop |
