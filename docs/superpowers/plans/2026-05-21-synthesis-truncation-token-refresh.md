# Synthesis Truncation + Token Refresh Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix two production bugs: (1) LLM synthesis hitting max_tokens produces a truncated response that fails JSON parsing; (2) installation tokens expire during long jobs causing 401 errors on post_outcome.

**Architecture:** Add `FinishReason` to `LlmResponse` so both providers surface stop reason; retry once on `Length` in `synthesize()`; fall back to a draft-based `UnifiedReview` on double truncation. In the dispatcher, re-mint a fresh GitHub client inside each checker task immediately before calling `post_outcome`, so expired tokens are replaced automatically.

**Tech Stack:** Rust, tokio, reqwest, wiremock 0.6, serde_json, thiserror, metrics crate, tracing.

---

## File Map

| File | Change |
|------|--------|
| `src/llm/mod.rs` | Add `FinishReason` enum; add `finish_reason: Option<FinishReason>` to `LlmResponse` |
| `src/llm/anthropic.rs` | Deserialize `stop_reason`; map to `FinishReason`; populate field |
| `src/llm/openai.rs` | Deserialize `finish_reason` from `choices[0]`; map and populate |
| `src/llm/factory.rs` | Log `finish_reason` in `TimedClient`'s completion log line |
| `src/checker/multi_review/synthesis.rs` | Add `SynthesisError::Truncated`; truncation check + one-shot retry in `synthesize()`; add `review_from_drafts()` helper |
| `src/checker/multi_review/orchestrator.rs` | Change `synthesize_for` return type to `Result<..., SynthesisError>`; handle `Truncated` in Barry R1 and `run_barry_only` |
| `src/dispatcher/run.rs` | Clone factory into each checker task; re-mint fresh client before `post_outcome` |

---

## Task 1: `FinishReason` enum + `LlmResponse` field

**Files:**
- Modify: `src/llm/mod.rs`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block at the bottom of `src/llm/mod.rs`:

```rust
    #[test]
    fn llm_response_has_finish_reason_field() {
        let r = LlmResponse {
            text: "hi".into(),
            input_tokens: None,
            output_tokens: None,
            finish_reason: Some(FinishReason::Length),
        };
        assert!(matches!(r.finish_reason, Some(FinishReason::Length)));
    }
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -q --lib llm::tests::llm_response_has_finish_reason_field
```
Expected: COMPILE ERROR — `FinishReason` is not defined and `LlmResponse` has no `finish_reason` field.

- [ ] **Step 3: Add `FinishReason` and update `LlmResponse`**

In `src/llm/mod.rs`, after the `Role` enum (around line 95), add:

```rust
/// Why the model stopped generating.
#[derive(Debug, Clone, PartialEq)]
pub enum FinishReason {
    /// Normal completion.
    Stop,
    /// Hit max_tokens — output was truncated.
    Length,
    /// Any other provider-specific reason.
    Other(String),
}

impl std::fmt::Display for FinishReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FinishReason::Stop => write!(f, "stop"),
            FinishReason::Length => write!(f, "length"),
            FinishReason::Other(s) => write!(f, "other:{s}"),
        }
    }
}
```

Replace the `LlmResponse` struct (around line 106):

```rust
/// Response from an LLM.
#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub text: String,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub finish_reason: Option<FinishReason>,
}
```

Note: remove `#[derive(Deserialize)]` since `FinishReason` does not derive it — `LlmResponse` is constructed by each provider, not deserialized directly.

- [ ] **Step 4: Fix all `LlmResponse` construction sites**

Three places construct `LlmResponse` without the new field and must add `finish_reason: None` as a placeholder (the providers will fill it in later tasks):

1. `src/llm/anthropic.rs` line ~107: add `finish_reason: None,`
2. `src/llm/openai.rs` line ~102: add `finish_reason: None,`
3. `src/checker/multi_review/orchestrator.rs` `ScriptedClient::complete` (in the test module, line ~384): add `finish_reason: None,`

Also update the `ScriptedClient` in `src/checker/multi_review/judge.rs` tests if it constructs `LlmResponse` directly — add `finish_reason: None,` there too.

- [ ] **Step 5: Run test to verify it passes**

```bash
cargo test -q --lib llm::tests::llm_response_has_finish_reason_field
```
Expected: PASS.

```bash
cargo test -q
```
Expected: all tests pass (the `None` placeholders are valid).

- [ ] **Step 6: Commit**

```bash
git add src/llm/mod.rs src/llm/anthropic.rs src/llm/openai.rs src/checker/multi_review/orchestrator.rs src/checker/multi_review/judge.rs
git commit -m "feat(llm): add FinishReason to LlmResponse

Providers will populate this field in subsequent commits. For now all
construction sites set finish_reason: None."
```

---

## Task 2: Anthropic captures `stop_reason`

**Files:**
- Modify: `src/llm/anthropic.rs`

- [ ] **Step 1: Write the failing test**

Add inside `#[cfg(test)] mod tests` in `src/llm/anthropic.rs`:

```rust
    #[tokio::test]
    async fn finish_reason_length_when_stop_reason_is_max_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [ { "type": "text", "text": "truncated" } ],
                "stop_reason": "max_tokens",
                "usage": { "input_tokens": 5, "output_tokens": 10 }
            })))
            .mount(&server)
            .await;
        let c = AnthropicClient::new(reqwest::Client::new(), server.uri(), None, "m".into());
        let r = c.complete(&LlmRequest {
            system: None,
            messages: vec![LlmMessage { role: Role::User, content: "go".into() }],
            max_tokens: 10,
            temperature: 0.0,
        }).await.unwrap();
        assert_eq!(r.finish_reason, Some(crate::llm::FinishReason::Length));
    }

    #[tokio::test]
    async fn finish_reason_stop_when_stop_reason_is_end_turn() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [ { "type": "text", "text": "done" } ],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 5, "output_tokens": 3 }
            })))
            .mount(&server)
            .await;
        let c = AnthropicClient::new(reqwest::Client::new(), server.uri(), None, "m".into());
        let r = c.complete(&LlmRequest {
            system: None,
            messages: vec![LlmMessage { role: Role::User, content: "go".into() }],
            max_tokens: 100,
            temperature: 0.0,
        }).await.unwrap();
        assert_eq!(r.finish_reason, Some(crate::llm::FinishReason::Stop));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -q --lib llm::anthropic::tests::finish_reason_length_when_stop_reason_is_max_tokens
```
Expected: FAIL — `finish_reason` is always `None`.

- [ ] **Step 3: Implement — deserialize `stop_reason` in `Resp`**

In `src/llm/anthropic.rs`, update the `Resp` struct to capture `stop_reason`:

```rust
#[derive(Deserialize)]
struct Resp {
    content: Vec<ContentBlock>,
    usage: Option<Usage>,
    stop_reason: Option<String>,
}
```

In `complete_once`, replace the `Ok(LlmResponse { ... })` construction at the end:

```rust
        let finish_reason = r.stop_reason.map(|s| match s.as_str() {
            "end_turn" => crate::llm::FinishReason::Stop,
            "max_tokens" => crate::llm::FinishReason::Length,
            other => crate::llm::FinishReason::Other(other.to_string()),
        });
        Ok(LlmResponse {
            text,
            input_tokens: r.usage.as_ref().and_then(|u| u.input_tokens),
            output_tokens: r.usage.as_ref().and_then(|u| u.output_tokens),
            finish_reason,
        })
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -q --lib llm::anthropic
```
Expected: all 3 tests pass (existing `calls_messages_endpoint` + 2 new).

- [ ] **Step 5: Commit**

```bash
git add src/llm/anthropic.rs
git commit -m "feat(llm/anthropic): capture stop_reason as FinishReason

Maps end_turn→Stop, max_tokens→Length, anything else→Other."
```

---

## Task 3: OpenAI captures `finish_reason`

**Files:**
- Modify: `src/llm/openai.rs`

- [ ] **Step 1: Write the failing tests**

Add inside `#[cfg(test)] mod tests` in `src/llm/openai.rs`:

```rust
    #[tokio::test]
    async fn finish_reason_length_when_finish_reason_is_length() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [ { "message": { "content": "truncated" }, "finish_reason": "length" } ],
                "usage": { "prompt_tokens": 5, "completion_tokens": 10 }
            })))
            .mount(&server)
            .await;
        let c = OpenAiClient::new(reqwest::Client::new(), server.uri(), None, "m".into());
        let r = c.complete(&LlmRequest {
            system: None,
            messages: vec![LlmMessage { role: Role::User, content: "go".into() }],
            max_tokens: 10,
            temperature: 0.0,
        }).await.unwrap();
        assert_eq!(r.finish_reason, Some(crate::llm::FinishReason::Length));
    }

    #[tokio::test]
    async fn finish_reason_stop_when_finish_reason_is_stop() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [ { "message": { "content": "done" }, "finish_reason": "stop" } ],
                "usage": { "prompt_tokens": 5, "completion_tokens": 3 }
            })))
            .mount(&server)
            .await;
        let c = OpenAiClient::new(reqwest::Client::new(), server.uri(), None, "m".into());
        let r = c.complete(&LlmRequest {
            system: None,
            messages: vec![LlmMessage { role: Role::User, content: "go".into() }],
            max_tokens: 100,
            temperature: 0.0,
        }).await.unwrap();
        assert_eq!(r.finish_reason, Some(crate::llm::FinishReason::Stop));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -q --lib llm::openai::tests::finish_reason_length_when_finish_reason_is_length
```
Expected: FAIL — `finish_reason` is always `None`.

- [ ] **Step 3: Implement — deserialize `finish_reason` in `Choice`**

In `src/llm/openai.rs`, update the `Choice` struct:

```rust
#[derive(Deserialize)]
struct Choice {
    message: Msg,
    finish_reason: Option<String>,
}
```

Replace the `Ok(LlmResponse { ... })` construction at the end of `complete_once`:

```rust
        let first = r.choices.into_iter().next()
            .ok_or_else(|| LlmError::Shape("no choices".into()))?;
        let finish_reason = first.finish_reason.map(|s| match s.as_str() {
            "stop" => crate::llm::FinishReason::Stop,
            "length" => crate::llm::FinishReason::Length,
            other => crate::llm::FinishReason::Other(other.to_string()),
        });
        Ok(LlmResponse {
            text: first.message.content,
            input_tokens: r.usage.as_ref().and_then(|u| u.prompt_tokens),
            output_tokens: r.usage.as_ref().and_then(|u| u.completion_tokens),
            finish_reason,
        })
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -q --lib llm::openai
```
Expected: all 5 tests pass (3 existing + 2 new).

- [ ] **Step 5: Commit**

```bash
git add src/llm/openai.rs
git commit -m "feat(llm/openai): capture finish_reason as FinishReason

Maps stop→Stop, length→Length, anything else→Other."
```

---

## Task 4: Factory logs `finish_reason`

**Files:**
- Modify: `src/llm/factory.rs`

No new test needed — the existing factory tests cover the happy path; finish_reason logging is a best-effort tracing side-effect.

- [ ] **Step 1: Update the completion log in `TimedClient::complete`**

In `src/llm/factory.rs`, replace the `Ok(resp)` arm of the `match &result` block (around line 113):

```rust
            Ok(resp) => {
                tracing::info!(
                    duration_ms,
                    input_tokens = resp.input_tokens,
                    output_tokens = resp.output_tokens,
                    finish_reason = resp.finish_reason.as_ref().map(|r| r.to_string()).as_deref().unwrap_or("unknown"),
                    "llm call completed"
                );
            }
```

- [ ] **Step 2: Verify it compiles and tests still pass**

```bash
cargo test -q --lib llm::factory
```
Expected: all tests pass.

- [ ] **Step 3: Commit**

```bash
git add src/llm/factory.rs
git commit -m "feat(llm/factory): log finish_reason in llm call completed

Surfaces truncation in the per-minute status ticker so operators can
see whether a long-running call is truncated without reading raw API logs."
```

---

## Task 5: `SynthesisError::Truncated` + retry + `review_from_drafts`

**Files:**
- Modify: `src/checker/multi_review/synthesis.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block at the bottom of `src/checker/multi_review/synthesis.rs`. First, check what's already there — if there is no test module yet, create one. Add:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{FinishReason, LlmClient, LlmError, LlmRequest, LlmResponse};
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct ScriptedClient(Mutex<Vec<LlmResponse>>);

    #[async_trait]
    impl LlmClient for ScriptedClient {
        async fn complete(&self, _req: &LlmRequest) -> Result<LlmResponse, LlmError> {
            let resp = self.0.lock().unwrap().remove(0);
            Ok(resp)
        }
    }

    fn truncated() -> LlmResponse {
        LlmResponse {
            text: "no json here, just gibberish that goes on forever".into(),
            input_tokens: Some(10),
            output_tokens: Some(1024),
            finish_reason: Some(FinishReason::Length),
        }
    }

    fn ok_review() -> LlmResponse {
        LlmResponse {
            text: r#"{"outcome":"approve","summary":"LGTM","findings":[]}"#.into(),
            input_tokens: Some(10),
            output_tokens: Some(50),
            finish_reason: Some(FinishReason::Stop),
        }
    }

    #[tokio::test]
    async fn synthesize_retries_once_on_length_then_succeeds() {
        let client = ScriptedClient(Mutex::new(vec![truncated(), ok_review()]));
        let (review, _) = synthesize(&client, &[], "diff", None, 1024).await.unwrap();
        assert_eq!(review.outcome, crate::checker::multi_review::review::Outcome::Approve);
    }

    #[tokio::test]
    async fn synthesize_returns_truncated_after_both_attempts_fail() {
        let client = ScriptedClient(Mutex::new(vec![truncated(), truncated()]));
        let err = synthesize(&client, &[], "diff", None, 1024).await.unwrap_err();
        assert!(matches!(err, SynthesisError::Truncated));
    }

    #[tokio::test]
    async fn synthesize_does_not_retry_on_stop() {
        // Only one response queued; if retry fires a second call it will panic on empty vec.
        let client = ScriptedClient(Mutex::new(vec![ok_review()]));
        let (review, _) = synthesize(&client, &[], "diff", None, 1024).await.unwrap();
        assert_eq!(review.outcome, crate::checker::multi_review::review::Outcome::Approve);
    }

    #[test]
    fn review_from_drafts_produces_comment_with_draft_text() {
        let drafts = vec![
            PersonaDraft {
                persona: "security",
                raw: "looks fine".into(),
                tokens: TokenCount::default(),
            },
            PersonaDraft {
                persona: "rust",
                raw: "idiomatic".into(),
                tokens: TokenCount::default(),
            },
        ];
        let r = review_from_drafts(&drafts);
        assert_eq!(r.outcome, crate::checker::multi_review::review::Outcome::Comment);
        assert!(r.summary.contains("security"));
        assert!(r.summary.contains("looks fine"));
        assert!(r.summary.contains("rust"));
        assert!(r.summary.contains("idiomatic"));
        assert!(r.findings.is_empty());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -q --lib checker::multi_review::synthesis::tests
```
Expected: COMPILE ERROR — `SynthesisError::Truncated` and `review_from_drafts` do not exist yet.

- [ ] **Step 3: Add `SynthesisError::Truncated`**

In `src/checker/multi_review/synthesis.rs`, update `SynthesisError`:

```rust
#[derive(Debug, thiserror::Error)]
pub enum SynthesisError {
    #[error("llm: {0}")]
    Llm(#[from] LlmError),
    #[error("parse: {0}")]
    Parse(#[from] ParseError),
    #[error("truncated: model hit max_tokens on both attempts")]
    Truncated,
}
```

- [ ] **Step 4: Add the truncation retry in `synthesize()`**

Replace the last three lines of `synthesize()` (the `let resp = ... ; Ok((parse(...)?, tokens))` block):

```rust
    let mut resp = client.complete(&req).await?;
    if matches!(resp.finish_reason, Some(crate::llm::FinishReason::Length)) {
        tracing::warn!(
            input_tokens = resp.input_tokens,
            output_tokens = resp.output_tokens,
            "synthesis response truncated at max_tokens; retrying once"
        );
        resp = client.complete(&req).await?;
        if matches!(resp.finish_reason, Some(crate::llm::FinishReason::Length)) {
            tracing::warn!("synthesis truncated on retry; giving up");
            return Err(SynthesisError::Truncated);
        }
    }
    let tokens = TokenCount {
        input: u64::from(resp.input_tokens.unwrap_or(0)),
        output: u64::from(resp.output_tokens.unwrap_or(0)),
    };
    Ok((parse(&resp.text)?, tokens))
```

The full updated `synthesize()` function (replacing lines 61–95 of the original):

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
    let mut resp = client.complete(&req).await?;
    if matches!(resp.finish_reason, Some(crate::llm::FinishReason::Length)) {
        tracing::warn!(
            input_tokens = resp.input_tokens,
            output_tokens = resp.output_tokens,
            "synthesis response truncated at max_tokens; retrying once"
        );
        resp = client.complete(&req).await?;
        if matches!(resp.finish_reason, Some(crate::llm::FinishReason::Length)) {
            tracing::warn!("synthesis truncated on retry; giving up");
            return Err(SynthesisError::Truncated);
        }
    }
    let tokens = TokenCount {
        input: u64::from(resp.input_tokens.unwrap_or(0)),
        output: u64::from(resp.output_tokens.unwrap_or(0)),
    };
    Ok((parse(&resp.text)?, tokens))
}
```

- [ ] **Step 5: Add `review_from_drafts()`**

After the `synthesize()` function (before `render_diff_block`), add:

```rust
/// Construct a minimal Comment-outcome review from raw persona draft text.
/// Used as a fallback when synthesis is truncated on both attempts.
pub fn review_from_drafts(drafts: &[PersonaDraft]) -> UnifiedReview {
    let summary = drafts
        .iter()
        .map(|d| format!("**{}**\n{}", d.persona, d.raw))
        .collect::<Vec<_>>()
        .join("\n\n");
    UnifiedReview {
        outcome: crate::checker::multi_review::review::Outcome::Comment,
        summary,
        findings: vec![],
    }
}
```

Note: `UnifiedReview` is already imported at the top of synthesis.rs via `use crate::checker::multi_review::review::{ParseError, UnifiedReview, parse};`.

- [ ] **Step 6: Run the tests to verify they pass**

```bash
cargo test -q --lib checker::multi_review::synthesis::tests
```
Expected: 4 tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/checker/multi_review/synthesis.rs
git commit -m "feat(synthesis): detect truncation, retry once, surface Truncated error

SynthesisError::Truncated is returned when the model hits max_tokens on
both the initial call and the one-shot retry. review_from_drafts() builds
a fallback Comment review from raw persona text for the orchestrator to
post when synthesis cannot complete."
```

---

## Task 6: Orchestrator handles `Truncated`

**Files:**
- Modify: `src/checker/multi_review/orchestrator.rs`

- [ ] **Step 1: Write the failing tests**

In the `#[cfg(test)] mod tests` block in `src/checker/multi_review/orchestrator.rs`, update `ScriptedClient` to support returning `finish_reason` and add two new tests.

First, update the `ScriptedClient` to hold `LlmResponse` directly instead of `Result<String, ...>`:

The existing `ScriptedClient` uses `Vec<Result<String, &'static str>>`. We need it to hold `Vec<Result<LlmResponse, &'static str>>` so we can set `finish_reason`. Replace the existing `ScriptedClient` and its helper `clients()` function with:

```rust
    struct ScriptedClient(Arc<Mutex<Vec<Result<LlmResponse, &'static str>>>>);

    #[async_trait]
    impl LlmClient for ScriptedClient {
        async fn complete(&self, _req: &LlmRequest) -> Result<LlmResponse, LlmError> {
            let next = self.0.lock().unwrap().pop();
            match next {
                Some(Ok(resp)) => Ok(resp),
                Some(Err(msg)) => Err(LlmError::Shape(msg.into())),
                None => Ok(LlmResponse {
                    text: r#"{"outcome":"approve","summary":"LGTM","findings":[]}"#.into(),
                    input_tokens: None,
                    output_tokens: None,
                    finish_reason: None,
                }),
            }
        }
    }

    fn ok_resp(text: &'static str) -> LlmResponse {
        LlmResponse { text: text.into(), input_tokens: None, output_tokens: None, finish_reason: None }
    }

    fn truncated_resp() -> LlmResponse {
        LlmResponse {
            text: "gibberish no json".into(),
            input_tokens: None,
            output_tokens: None,
            finish_reason: Some(crate::llm::FinishReason::Length),
        }
    }

    fn clients(
        barry: Vec<Result<LlmResponse, &'static str>>,
        ob: Vec<Result<LlmResponse, &'static str>>,
        judge: Vec<Result<LlmResponse, &'static str>>,
    ) -> IdentityClients {
        let to_owned = |v: Vec<Result<LlmResponse, &'static str>>| {
            Arc::new(Mutex::new(v))
        };
        IdentityClients {
            barry: Arc::new(ScriptedClient(to_owned(barry))),
            other_barry: Arc::new(ScriptedClient(to_owned(ob))),
            other_other_barry: Arc::new(ScriptedClient(to_owned(vec![]))),
            judge: Arc::new(ScriptedClient(to_owned(judge))),
            barry_max_tokens: 1024,
            other_barry_max_tokens: 1024,
            other_other_barry_max_tokens: 1024,
            judge_max_tokens: 256,
        }
    }
```

Update the helper functions `approve()`, `comment()`, `agree()`, `disagree()` to return `LlmResponse` via `ok_resp`:

```rust
    fn approve() -> LlmResponse { ok_resp(r#"{"outcome":"approve","summary":"LGTM","findings":[]}"#) }
    fn comment() -> LlmResponse { ok_resp(r#"{"outcome":"comment","summary":"check this","findings":[]}"#) }
    fn agree() -> LlmResponse { ok_resp(r#"{"agree":true,"reason":"same"}"#) }
    fn disagree() -> LlmResponse { ok_resp(r#"{"agree":false,"reason":"diff"}"#) }
```

Update existing test `clients(...)` calls — each call like `vec![Ok(approve()), Ok(approve()), ...]` becomes `vec![Ok(approve()), Ok(approve()), ...]` (same shape, just `LlmResponse` instead of `&'static str`).

Then add two new tests:

```rust
    #[tokio::test]
    async fn barry_r1_truncation_returns_barry_alone_with_draft_content() {
        // Barry: 2 persona drafts (approve), then 2 truncated R1 synths (retry fires once).
        // OB: 2 persona drafts. Neither OB synth nor judge should be called.
        let c = clients(
            vec![
                Ok(truncated_resp()), // R1 synth retry
                Ok(truncated_resp()), // R1 synth first attempt
                Ok(approve()),        // rust draft
                Ok(approve()),        // security draft
            ],
            vec![Ok(approve()), Ok(approve())], // OB drafts (run in parallel, wasted)
            vec![],                              // judge must not be called
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
            Verdict::BarryAlone { barry, reason } => {
                assert_eq!(reason, "synthesis truncated");
                // from_drafts produces Comment outcome
                assert_eq!(barry.outcome, crate::checker::multi_review::review::Outcome::Comment);
            }
            other => panic!("wanted BarryAlone, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_barry_only_truncation_returns_barry_alone_with_draft_content() {
        // Barry: 2 persona drafts, then 2 truncated R1 synths.
        let c = clients(
            vec![
                Ok(truncated_resp()), // R1 synth retry
                Ok(truncated_resp()), // R1 synth first attempt
                Ok(approve()),        // rust draft
                Ok(approve()),        // security draft
            ],
            vec![], // OB not called
            vec![], // judge not called
        );
        let p = personas();
        let v = Orchestrator {
            clients: &c,
            personas: &p,
            tracker: Arc::new(StatusTracker::new()),
            job_id: 0,
        }
        .run_barry_only(&[file()], "OB not installed".into())
        .await
        .unwrap();
        match v {
            Verdict::BarryAlone { barry, reason } => {
                // reason from run_barry_only input is overridden when truncation fires
                assert_eq!(reason, "synthesis truncated");
                assert_eq!(barry.outcome, crate::checker::multi_review::review::Outcome::Comment);
            }
            other => panic!("wanted BarryAlone, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -q --lib checker::multi_review::orchestrator::tests
```
Expected: compile errors or test failures — the orchestrator doesn't yet handle `SynthesisError::Truncated`.

- [ ] **Step 3: Change `synthesize_for` to return `Result<..., SynthesisError>`**

In `src/checker/multi_review/orchestrator.rs`, add to the imports at the top:

```rust
use crate::checker::multi_review::synthesis::{self, PersonaDraft, SynthesisError, TokenCount};
```

(replace the existing `synthesis::{self, PersonaDraft, TokenCount}` import)

Also add:

```rust
use crate::checker::multi_review::review;
```

Change `synthesize_for` signature and body:

```rust
    async fn synthesize_for(
        &self,
        identity: Identity,
        diff: &str,
        drafts: &[PersonaDraft],
        peer: Option<&str>,
    ) -> Result<(UnifiedReview, TokenCount), SynthesisError> {
        let round = if peer.is_some() { "R2" } else { "R1" };
        let client = self.clients.for_identity(identity);
        let max_tokens = self.clients.max_tokens_for(identity);
        let start = std::time::Instant::now();

        let result = synthesis::synthesize(client.as_ref(), drafts, diff, peer, max_tokens).await;

        let duration_ms = start.elapsed().as_millis() as u64;
        match result {
            Ok((r, tokens)) => {
                tracing::info!(
                    identity = ?identity,
                    round,
                    duration_ms,
                    outcome = format!("{:?}", r.outcome),
                    "synthesis done"
                );
                Ok((r, tokens))
            }
            Err(e) => Err(e),
        }
    }
```

- [ ] **Step 4: Handle `SynthesisError::Truncated` in `run()`**

In the `ob_drafts` failure arm (around line 82), the inner `synthesize_for` call for the Barry-alone fallback path must handle `Truncated`. Replace lines 82-84:

```rust
                let (barry_r1, r1_tokens) = match self
                    .synthesize_for(Identity::Barry, &diff, &barry_drafts, None)
                    .await
                {
                    Ok(t) => t,
                    Err(SynthesisError::Truncated) => {
                        tracing::warn!("barry R1 synthesis truncated; using persona-draft fallback");
                        metrics::counter!("barry_multi_review_truncated_total", "phase" => "r1_synthesis").increment(1);
                        return Ok(Verdict::BarryAlone {
                            barry: synthesis::review_from_drafts(&barry_drafts),
                            reason: "synthesis truncated".into(),
                        });
                    }
                    Err(e) => return Err(anyhow::anyhow!("barry R1 failed: {e}")),
                };
```

Replace the main Barry R1 match arm (around line 116-119):

```rust
        let (barry_r1, barry_r1_tokens) = match barry_r1_res {
            Ok(t) => t,
            Err(SynthesisError::Truncated) => {
                tracing::warn!("barry R1 synthesis truncated; using persona-draft fallback");
                metrics::counter!("barry_multi_review_truncated_total", "phase" => "r1_synthesis").increment(1);
                return Ok(Verdict::BarryAlone {
                    barry: synthesis::review_from_drafts(&barry_drafts),
                    reason: "synthesis truncated".into(),
                });
            }
            Err(e) => return Err(anyhow::anyhow!("barry R1 failed: {e}")),
        };
```

The OB R1 match arm (around line 120-133) already uses `Err(e)` which now receives `SynthesisError` — update it:

```rust
        let (ob_r1, ob_r1_tokens) = match ob_r1_res {
            Ok(t) => t,
            Err(e) => {
                self.tracker
                    .add_tokens(self.job_id, barry_r1_tokens.input, barry_r1_tokens.output);
                tracing::warn!(?e, "Other Barry R1 synthesis failed; Barry posts alone");
                tracing::info!(kind = "barry_alone", "verdict");
                metrics::counter!("barry_multi_review_barry_alone_total").increment(1);
                return Ok(Verdict::BarryAlone {
                    barry: barry_r1,
                    reason: format!("Other Barry unavailable: {e}"),
                });
            }
        };
```

The R2 arms already use `Err(_)` wildcards — they still compile as-is.

- [ ] **Step 5: Handle `SynthesisError::Truncated` in `run_barry_only()`**

Replace lines 261-264 in `run_barry_only`:

```rust
        let (barry_r1, r1_tokens) = match self
            .synthesize_for(Identity::Barry, &diff, &barry_drafts, None)
            .await
        {
            Ok(t) => t,
            Err(SynthesisError::Truncated) => {
                tracing::warn!("barry R1 synthesis truncated in run_barry_only; using persona-draft fallback");
                metrics::counter!("barry_multi_review_truncated_total", "phase" => "r1_synthesis").increment(1);
                return Ok(Verdict::BarryAlone {
                    barry: synthesis::review_from_drafts(&barry_drafts),
                    reason: "synthesis truncated".into(),
                });
            }
            Err(e) => return Err(anyhow::anyhow!("barry R1 failed: {e}")),
        };
```

- [ ] **Step 6: Run the tests to verify they pass**

```bash
cargo test -q --lib checker::multi_review::orchestrator
```
Expected: all tests pass (existing 4 + 2 new).

- [ ] **Step 7: Commit**

```bash
git add src/checker/multi_review/orchestrator.rs
git commit -m "feat(orchestrator): fall back to persona drafts on synthesis truncation

When Barry's R1 synthesis hits max_tokens on both attempts, the
orchestrator returns BarryAlone with a review built from the raw persona
drafts rather than failing the checker entirely. Emits
barry_multi_review_truncated_total{phase=r1_synthesis} for monitoring."
```

---

## Task 7: Token refresh before `post_outcome`

**Files:**
- Modify: `src/dispatcher/run.rs`

The `gh` client is created at job start and reused for all `post_outcome` calls. Jobs longer than ~60 minutes outlast the installation token. Fix: clone the factory into each checker task and re-mint immediately before calling `post_outcome`.

- [ ] **Step 1: Write a compile-check (no test possible without infra)**

No unit test can exercise token expiry (no mocking harness for the GH factory in run_job). The change is verified by compiling and confirming existing tests pass.

- [ ] **Step 2: Add factory capture into the task loop**

In `src/dispatcher/run.rs`, inside `run_job()`, locate the `for chk in &deps.pipeline.checkers {` loop (around line 169). Before the loop, add:

```rust
    let gh_factory = deps.gh_factory.clone();
    let installation_id = job.installation_id;
```

Inside the `async move` task closure, before the `post_outcome` call, add the re-mint:

Locate the existing line:
```rust
                if let Err(e) = post_outcome(gh_ref, job_ref, pr_ref, &outcome, &cancel).await {
```

Replace it with:

```rust
                // Re-mint token: long-running jobs may have outlasted the 1h installation
                // token minted at job start. get_or_mint_for uses a cached value when
                // possible (5-min expiry buffer), so this is a cheap DB read in the
                // common case.
                let post_gh = match gh_factory.for_installation(installation_id).await {
                    Ok(g) => g,
                    Err(e) => {
                        tracing::warn!(?e, "failed to re-mint token before post_outcome; using original");
                        Arc::clone(gh_ref)
                    }
                };
                if let Err(e) = post_outcome(&post_gh, job_ref, pr_ref, &outcome, &cancel).await {
```

Also add the factory capture inside the loop, before `tasks.push(...)`:

```rust
        let gh_factory = gh_factory.clone(); // each task gets its own Arc clone
```

The complete task creation block for each checker now looks like:

```rust
    let gh_factory = deps.gh_factory.clone();
    let installation_id = job.installation_id;
    // ...
    for chk in &deps.pipeline.checkers {
        if !chk.enabled(&ctx.repo_cfg) { continue; }
        let chk = chk.clone();
        let rate_limit_reset = rate_limit_reset.clone();
        let checker_name = chk.name();
        let cancel = cancel.clone();
        let gh_factory = gh_factory.clone();
        let span = tracing::info_span!(...);
        tasks.push(
            async move {
                // ...existing checker run...

                // Re-mint before posting.
                let post_gh = match gh_factory.for_installation(installation_id).await {
                    Ok(g) => g,
                    Err(e) => {
                        tracing::warn!(?e, "failed to re-mint token before post_outcome; using original");
                        Arc::clone(gh_ref)
                    }
                };
                if let Err(e) = post_outcome(&post_gh, job_ref, pr_ref, &outcome, &cancel).await {
                    // ...existing error handling unchanged...
                }
                // ...rest unchanged...
            }
            .instrument(span),
        );
    }
```

- [ ] **Step 3: Compile and run all tests**

```bash
cargo build 2>&1 | head -20
cargo test -q
```
Expected: clean build, all tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/dispatcher/run.rs
git commit -m "fix(dispatcher): re-mint installation token before post_outcome

Jobs running longer than ~60 minutes exhausted the installation token
minted at job start, causing 401 Bad credentials on post_outcome. Now
each checker task calls for_installation() immediately before posting —
get_or_mint_for checks expiry (with a 5-min buffer) and re-mints only
when needed, so this is a cheap DB read in the common case."
```

---

## Task 8: Full suite + fmt + clippy

**Files:** (none — verification)

- [ ] **Step 1: Format**

```bash
cargo fmt
```

- [ ] **Step 2: Clippy**

```bash
cargo clippy --all-targets -- -D warnings
```
Expected: no warnings. If any appear, fix them inline (likely: unused imports, needless borrows from the `Arc::clone(gh_ref)` fallback path). Do not suppress with `#[allow]` unless well-justified.

- [ ] **Step 3: Full test sweep**

```bash
cargo test -q
```
Expected: all tests pass.

- [ ] **Step 4: Commit any fixups**

```bash
git status
# If there are changes:
git add -A
git commit -m "chore: cargo fmt + clippy fixups"
```

- [ ] **Step 5: Review the branch**

```bash
git log --oneline main..HEAD
git diff --stat main..HEAD
```
Expected: 7–8 commits covering Tasks 1–7 (plus optional cleanup), touching 7 files.
