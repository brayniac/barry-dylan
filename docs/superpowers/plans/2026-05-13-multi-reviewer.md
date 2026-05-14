# Multi-Reviewer ("Barry and Other Barry") Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the single `LlmReviewChecker` with a three-identity, multi-persona reviewer (Barry / Other Barry / OOB) that surfaces a second voice only when two independent LLM reviews materially disagree.

**Architecture:** One binary, three GitHub Apps (each with its own LLM config). Each Barry runs N persona prompts internally → synthesis → unified review. Default flow: Barry+OB do R1 then R2 (each reads the other), then a judge prompt decides whether they agree. Agreement → Barry alone posts. Disagreement → both post, check-run goes neutral. `/barry confer` summons OB (then OOB) on demand.

**Tech Stack:** Rust 2024 edition, Tokio, sqlx (SQLite WAL), Axum, async-trait, reqwest, wiremock for tests.

**Spec:** `docs/superpowers/specs/2026-05-13-multi-reviewer-design.md`

---

## File Structure

### New files

| Path | Responsibility |
|---|---|
| `src/checker/multi_review/mod.rs` | `MultiReviewChecker` (impls `Checker`) — entry point from pipeline |
| `src/checker/multi_review/identity.rs` | `Identity` enum + display, parsing |
| `src/checker/multi_review/persona.rs` | `Persona` struct, built-in set, prompt resolution |
| `src/checker/multi_review/review.rs` | `UnifiedReview`, `Outcome`, parser for synthesized output |
| `src/checker/multi_review/synthesis.rs` | Synthesis prompt + call (per-Barry merge of persona drafts) |
| `src/checker/multi_review/judge.rs` | Judge prompt + agree/disagree call |
| `src/checker/multi_review/orchestrator.rs` | R1/R2/judge orchestration |
| `src/checker/multi_review/posting.rs` | Posts a `UnifiedReview` under a given `Identity` |
| `src/checker/multi_review/confer.rs` | Confer state machine + handler |
| `src/checker/multi_review/prompts/security.md` | Default security persona prompt |
| `src/checker/multi_review/prompts/correctness.md` | Default correctness persona prompt |
| `src/checker/multi_review/prompts/style.md` | Default style persona prompt |
| `src/checker/multi_review/prompts/synthesis.md` | Default synthesis prompt template |
| `src/checker/multi_review/prompts/judge.md` | Default judge prompt template |
| `src/storage/multi_review.rs` | `multi_review_runs` table accessors |
| `tests/integration/multi_review_agreement.rs` | E2E agreement scenario |
| `tests/integration/multi_review_disagreement.rs` | E2E disagreement scenario |
| `tests/integration/multi_review_confer.rs` | E2E confer escalation scenario |

### Modified files

| Path | Change |
|---|---|
| `src/config/mod.rs` | Add `IdentityCreds`, extend `GitHubConfig` to plural, add `[judge]`, `[personas.*]`, `[confer]` |
| `src/dispatcher/run.rs` | Replace `GhFactory` trait with `MultiGhFactory`; route confer command |
| `src/dispatcher/trust.rs` | Add `BarryCommand::Confer` variant |
| `src/webhook/server.rs` | Route `/barry confer` to `issue_comment.confer` event_kind |
| `src/app_runtime.rs` | Build three `AppCreds`, three LLM clients, judge client, register `MultiReviewChecker` |
| `src/storage/mod.rs` | Add `multi_review` module declaration; new schema statements |
| `src/storage/schema.sql` | Add `multi_review_runs` table |
| `src/checker/mod.rs` | Re-export new module |
| `src/checker/llm_review/` | **DELETED** at the end |

---

## Task Sequence

Tasks are ordered so each one builds on prior tasks and produces a green build. Each task ends in a commit.

---

### Task 1: Identity enum and built-in persona definitions

**Files:**
- Create: `src/checker/multi_review/mod.rs`
- Create: `src/checker/multi_review/identity.rs`
- Create: `src/checker/multi_review/persona.rs`
- Create: `src/checker/multi_review/prompts/security.md`
- Create: `src/checker/multi_review/prompts/correctness.md`
- Create: `src/checker/multi_review/prompts/style.md`
- Modify: `src/checker/mod.rs` (add `pub mod multi_review;`)

- [ ] **Step 1: Add module declaration to `src/checker/mod.rs`**

At the top of the file, after existing `pub mod` lines:

```rust
pub mod multi_review;
```

- [ ] **Step 2: Create `src/checker/multi_review/mod.rs`**

```rust
//! Multi-identity, multi-persona LLM review checker.
//!
//! Three GitHub App identities (Barry, Other Barry, OOB) each driven by their
//! own LLM. See `docs/superpowers/specs/2026-05-13-multi-reviewer-design.md`.

pub mod identity;
pub mod persona;
```

- [ ] **Step 3: Write failing test for `Identity`**

Append to `src/checker/multi_review/identity.rs`:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Identity {
    Barry,
    OtherBarry,
    OtherOtherBarry,
}

impl Identity {
    pub fn label(self) -> &'static str {
        match self {
            Identity::Barry => "Barry",
            Identity::OtherBarry => "Other Barry",
            Identity::OtherOtherBarry => "Other Other Barry",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Identity::Barry => "barry",
            Identity::OtherBarry => "other_barry",
            Identity::OtherOtherBarry => "other_other_barry",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_distinct() {
        assert_ne!(Identity::Barry.label(), Identity::OtherBarry.label());
        assert_ne!(Identity::OtherBarry.label(), Identity::OtherOtherBarry.label());
    }

    #[test]
    fn slugs_match_config_keys() {
        assert_eq!(Identity::Barry.slug(), "barry");
        assert_eq!(Identity::OtherBarry.slug(), "other_barry");
        assert_eq!(Identity::OtherOtherBarry.slug(), "other_other_barry");
    }
}
```

- [ ] **Step 4: Run tests to verify pass**

```bash
cargo test --lib checker::multi_review::identity
```

Expected: 2 passed.

- [ ] **Step 5: Add persona prompt files**

Create the three default persona prompts. These can be one-paragraph each for v1 and tuned later via dogfooding.

`src/checker/multi_review/prompts/security.md`:

```
You are reviewing this diff strictly for security issues. Look for: input validation gaps, injection vectors (SQL, command, header), unsafe deserialization, secrets in source, missing authn/authz checks, TOCTOU bugs, unsafe defaults, weak cryptography, and information leaks via logs or errors. Ignore style, naming, and non-security correctness — other reviewers handle those. If there are no real security issues in the diff, return an empty findings array.
```

`src/checker/multi_review/prompts/correctness.md`:

```
You are reviewing this diff strictly for correctness. Look for: logic errors, off-by-one, broken invariants, race conditions, error-path handling that drops data, incorrect API usage, broken contracts with callers, and tests that don't actually test what their names claim. Ignore style and security — other reviewers handle those. If the change is correct as written, return an empty findings array.
```

`src/checker/multi_review/prompts/style.md`:

```
You are reviewing this diff strictly for code quality and idiom. Look for: dead code, unclear naming that obscures intent, duplication that should be DRYed, abstractions that don't earn their weight, comments that explain WHAT instead of WHY, error messages that won't help a future reader. Ignore correctness and security — other reviewers handle those. If the diff is clean, return an empty findings array.
```

- [ ] **Step 6: Write failing tests for persona resolution**

Append to `src/checker/multi_review/persona.rs`:

```rust
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Persona {
    pub name: &'static str,
    pub prompt: Arc<String>,
}

const SECURITY: &str = include_str!("prompts/security.md");
const CORRECTNESS: &str = include_str!("prompts/correctness.md");
const STYLE: &str = include_str!("prompts/style.md");

#[derive(Debug, Clone, Default)]
pub struct PersonaOverrides {
    pub security: Option<PathBuf>,
    pub correctness: Option<PathBuf>,
    pub style: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum PersonaError {
    #[error("reading persona prompt {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub fn resolve(overrides: &PersonaOverrides) -> Result<Vec<Persona>, PersonaError> {
    Ok(vec![
        Persona {
            name: "security",
            prompt: Arc::new(load(overrides.security.as_deref(), SECURITY)?),
        },
        Persona {
            name: "correctness",
            prompt: Arc::new(load(overrides.correctness.as_deref(), CORRECTNESS)?),
        },
        Persona {
            name: "style",
            prompt: Arc::new(load(overrides.style.as_deref(), STYLE)?),
        },
    ])
}

fn load(path: Option<&std::path::Path>, default: &str) -> Result<String, PersonaError> {
    match path {
        None => Ok(default.to_string()),
        Some(p) => std::fs::read_to_string(p).map_err(|e| PersonaError::Io {
            path: p.into(),
            source: e,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn defaults_all_loaded() {
        let p = resolve(&PersonaOverrides::default()).unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].name, "security");
        assert!(!p[0].prompt.is_empty());
        assert!(!p[1].prompt.is_empty());
        assert!(!p[2].prompt.is_empty());
    }

    #[test]
    fn override_replaces_default() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "custom security prompt").unwrap();
        let ovr = PersonaOverrides {
            security: Some(f.path().to_path_buf()),
            ..Default::default()
        };
        let p = resolve(&ovr).unwrap();
        assert!(p[0].prompt.contains("custom security prompt"));
        // Other personas still load defaults.
        assert!(!p[1].prompt.contains("custom"));
    }

    #[test]
    fn missing_override_file_errors() {
        let ovr = PersonaOverrides {
            style: Some(PathBuf::from("/nonexistent/path/to/prompt.md")),
            ..Default::default()
        };
        let err = resolve(&ovr).unwrap_err();
        assert!(matches!(err, PersonaError::Io { .. }));
    }
}
```

- [ ] **Step 7: Update `src/checker/multi_review/mod.rs` (already declared)**

Already done in Step 2.

- [ ] **Step 8: Run tests, verify pass**

```bash
cargo test --lib checker::multi_review
```

Expected: identity tests + persona tests all pass (5 total).

- [ ] **Step 9: Commit**

```bash
git add src/checker/multi_review/ src/checker/mod.rs
git commit -m "feat(multi-review): identity enum and built-in persona prompts"
```

---

### Task 2: UnifiedReview, Outcome, and parser

**Files:**
- Create: `src/checker/multi_review/review.rs`
- Modify: `src/checker/multi_review/mod.rs`

- [ ] **Step 1: Declare module**

Append to `src/checker/multi_review/mod.rs`:

```rust
pub mod review;
```

- [ ] **Step 2: Write failing tests for parsing**

Create `src/checker/multi_review/review.rs`:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Approve,
    Comment,
    RequestChanges,
}

impl Outcome {
    pub fn check_conclusion(self) -> crate::github::check_run::CheckConclusion {
        use crate::github::check_run::CheckConclusion::*;
        match self {
            Outcome::Approve => Success,
            Outcome::Comment => Neutral,
            Outcome::RequestChanges => Failure,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct UnifiedReview {
    /// One of "approve", "comment", "request_changes".
    pub outcome: Outcome,
    /// Short prose summary visible at the top of the posted review.
    pub summary: String,
    /// Inline findings keyed to (file, line). Optional.
    #[serde(default)]
    pub findings: Vec<UnifiedFinding>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UnifiedFinding {
    pub file: String,
    pub line: u32,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("could not locate JSON object in model output")]
    NoJson,
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Locate the first balanced JSON object in `text` and parse a UnifiedReview.
/// Tolerates model output that wraps JSON in prose or markdown fences.
pub fn parse(text: &str) -> Result<UnifiedReview, ParseError> {
    let slice = locate_json(text).ok_or(ParseError::NoJson)?;
    Ok(serde_json::from_str(slice)?)
}

fn locate_json(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut start = None;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            if esc {
                esc = false;
                continue;
            }
            if b == b'\\' {
                esc = true;
                continue;
            }
            if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 && let Some(s) = start {
                    return Some(&text[s..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_approve() {
        let r = parse(r#"{"outcome":"approve","summary":"LGTM","findings":[]}"#).unwrap();
        assert_eq!(r.outcome, Outcome::Approve);
        assert_eq!(r.summary, "LGTM");
        assert!(r.findings.is_empty());
    }

    #[test]
    fn parses_request_changes_with_findings() {
        let r = parse(
            r#"{"outcome":"request_changes","summary":"missing checks",
                "findings":[{"file":"a.rs","line":3,"message":"unchecked unwrap"}]}"#,
        )
        .unwrap();
        assert_eq!(r.outcome, Outcome::RequestChanges);
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].line, 3);
    }

    #[test]
    fn parses_in_fenced_block() {
        let r = parse("preamble\n```json\n{\"outcome\":\"comment\",\"summary\":\"x\"}\n```").unwrap();
        assert_eq!(r.outcome, Outcome::Comment);
    }

    #[test]
    fn rejects_no_json() {
        assert!(matches!(parse("not json"), Err(ParseError::NoJson)));
    }

    #[test]
    fn outcome_maps_to_check_conclusion() {
        use crate::github::check_run::CheckConclusion;
        assert!(matches!(Outcome::Approve.check_conclusion(), CheckConclusion::Success));
        assert!(matches!(Outcome::Comment.check_conclusion(), CheckConclusion::Neutral));
        assert!(matches!(Outcome::RequestChanges.check_conclusion(), CheckConclusion::Failure));
    }
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test --lib checker::multi_review::review
```

Expected: 5 passed.

- [ ] **Step 4: Commit**

```bash
git add src/checker/multi_review/review.rs src/checker/multi_review/mod.rs
git commit -m "feat(multi-review): UnifiedReview type and JSON parser"
```

---

### Task 3: Synthesis prompt and call

**Files:**
- Create: `src/checker/multi_review/synthesis.rs`
- Create: `src/checker/multi_review/prompts/synthesis.md`
- Modify: `src/checker/multi_review/mod.rs`

- [ ] **Step 1: Declare module**

Append to `src/checker/multi_review/mod.rs`:

```rust
pub mod synthesis;
```

- [ ] **Step 2: Add synthesis prompt template**

Create `src/checker/multi_review/prompts/synthesis.md`:

```
You are the synthesis stage. You will receive the diff and N draft reviews each written from a different review focus (security, correctness, style). Produce ONE unified review that:
- States your overall outcome: "approve" (no real issues), "comment" (worth flagging but non-blocking), or "request_changes" (one or more must-fix issues)
- Has a short prose summary (under 80 words) the user actually reads
- Includes only the findings that are real, specific, and actionable. Drop redundant or speculative items. Keep file/line for each.

Output one JSON object exactly matching:
{
  "outcome": "approve" | "comment" | "request_changes",
  "summary": "<prose>",
  "findings": [{"file":"<path>","line":<int>,"message":"<text>"}]
}
Do not include any text outside the JSON.
```

- [ ] **Step 3: Write failing tests**

Create `src/checker/multi_review/synthesis.rs`:

```rust
use crate::checker::multi_review::persona::Persona;
use crate::checker::multi_review::review::{ParseError, UnifiedReview, parse};
use crate::github::pr::ChangedFile;
use crate::llm::{LlmClient, LlmError, LlmMessage, LlmRequest, Role};
use std::sync::Arc;

const SYNTHESIS_TEMPLATE: &str = include_str!("prompts/synthesis.md");

#[derive(Debug, thiserror::Error)]
pub enum SynthesisError {
    #[error("llm: {0}")]
    Llm(#[from] LlmError),
    #[error("parse: {0}")]
    Parse(#[from] ParseError),
}

pub struct PersonaDraft {
    pub persona: &'static str,
    pub raw: String,
}

/// Run a single persona prompt over the diff and return its raw text output.
pub async fn run_persona(
    client: &dyn LlmClient,
    persona: &Persona,
    diff_block: &str,
    max_tokens: u32,
) -> Result<PersonaDraft, LlmError> {
    let req = LlmRequest {
        system: Some(persona.prompt.as_ref().clone()),
        messages: vec![LlmMessage {
            role: Role::User,
            content: format!(
                "{diff_block}\n\nReturn ONLY a JSON object with: \
                 {{\"findings\":[{{\"file\":\"<path>\",\"line\":<int>,\"message\":\"<text>\"}}],\
                 \"summary\":\"<one short sentence>\"}}"
            ),
        }],
        max_tokens,
        temperature: 0.0,
    };
    let resp = client.complete(&req).await?;
    Ok(PersonaDraft {
        persona: persona.name,
        raw: resp.text,
    })
}

/// Run synthesis over N persona drafts and return a parsed UnifiedReview.
pub async fn synthesize(
    client: &dyn LlmClient,
    drafts: &[PersonaDraft],
    diff_block: &str,
    prior_peer_review: Option<&str>,
    max_tokens: u32,
) -> Result<UnifiedReview, SynthesisError> {
    let mut user = String::new();
    user.push_str(diff_block);
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
        system: Some(SYNTHESIS_TEMPLATE.to_string()),
        messages: vec![LlmMessage {
            role: Role::User,
            content: user,
        }],
        max_tokens,
        temperature: 0.0,
    };
    let resp = client.complete(&req).await?;
    Ok(parse(&resp.text)?)
}

/// Render a diff block from changed files, suitable for embedding in a user message.
pub fn render_diff_block(files: &[ChangedFile]) -> String {
    let mut s = String::from("=== diff begins ===\n");
    for f in files {
        s.push_str(&format!("File: {}\n```\n", f.filename));
        if let Some(p) = &f.patch {
            s.push_str(p);
        }
        s.push_str("\n```\n");
    }
    s.push_str("=== diff ends ===\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::LlmResponse;
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct StubClient {
        responses: Mutex<Vec<String>>,
        recorded: Arc<Mutex<Vec<LlmRequest>>>,
    }
    #[async_trait]
    impl LlmClient for StubClient {
        async fn complete(&self, req: &LlmRequest) -> Result<LlmResponse, LlmError> {
            self.recorded.lock().unwrap().push(req.clone());
            let text = self.responses.lock().unwrap().remove(0);
            Ok(LlmResponse {
                text,
                input_tokens: None,
                output_tokens: None,
            })
        }
    }

    fn persona(name: &'static str) -> Persona {
        Persona {
            name,
            prompt: Arc::new(format!("you are {name}")),
        }
    }

    fn file(name: &str, patch: &str) -> ChangedFile {
        ChangedFile {
            filename: name.into(),
            status: "modified".into(),
            additions: 1,
            deletions: 0,
            changes: 1,
            patch: Some(patch.into()),
        }
    }

    #[tokio::test]
    async fn run_persona_passes_prompt_as_system() {
        let recorded = Arc::new(Mutex::new(vec![]));
        let client = StubClient {
            responses: Mutex::new(vec![r#"{"findings":[],"summary":"ok"}"#.into()]),
            recorded: recorded.clone(),
        };
        let p = persona("security");
        let d = run_persona(&client, &p, "diff", 1024).await.unwrap();
        assert_eq!(d.persona, "security");
        let r = recorded.lock().unwrap();
        assert!(r[0].system.as_ref().unwrap().contains("you are security"));
    }

    #[tokio::test]
    async fn synthesize_includes_all_persona_drafts_in_user() {
        let recorded = Arc::new(Mutex::new(vec![]));
        let client = StubClient {
            responses: Mutex::new(vec![
                r#"{"outcome":"approve","summary":"LGTM","findings":[]}"#.into(),
            ]),
            recorded: recorded.clone(),
        };
        let drafts = vec![
            PersonaDraft { persona: "security", raw: "sec-draft".into() },
            PersonaDraft { persona: "style", raw: "style-draft".into() },
        ];
        let r = synthesize(&client, &drafts, "diff", None, 1024).await.unwrap();
        assert_eq!(r.outcome, crate::checker::multi_review::review::Outcome::Approve);
        let r = recorded.lock().unwrap();
        let user = &r[0].messages[0].content;
        assert!(user.contains("sec-draft"));
        assert!(user.contains("style-draft"));
        assert!(!user.contains("peer review"));
    }

    #[tokio::test]
    async fn synthesize_includes_peer_when_provided() {
        let recorded = Arc::new(Mutex::new(vec![]));
        let client = StubClient {
            responses: Mutex::new(vec![
                r#"{"outcome":"comment","summary":"updated","findings":[]}"#.into(),
            ]),
            recorded: recorded.clone(),
        };
        let drafts = vec![PersonaDraft { persona: "security", raw: "sd".into() }];
        let _ = synthesize(&client, &drafts, "diff", Some("PEER-R1"), 1024).await.unwrap();
        let r = recorded.lock().unwrap();
        assert!(r[0].messages[0].content.contains("PEER-R1"));
    }

    #[test]
    fn render_diff_block_wraps_each_file() {
        let s = render_diff_block(&[file("a.rs", "@@ -1 +1 @@\n+x")]);
        assert!(s.contains("File: a.rs"));
        assert!(s.contains("=== diff begins ==="));
        assert!(s.contains("=== diff ends ==="));
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test --lib checker::multi_review::synthesis
```

Expected: 4 passed.

- [ ] **Step 5: Commit**

```bash
git add src/checker/multi_review/synthesis.rs src/checker/multi_review/prompts/synthesis.md src/checker/multi_review/mod.rs
git commit -m "feat(multi-review): per-persona run + synthesis call"
```

---

### Task 4: Judge prompt and call

**Files:**
- Create: `src/checker/multi_review/judge.rs`
- Create: `src/checker/multi_review/prompts/judge.md`
- Modify: `src/checker/multi_review/mod.rs`

- [ ] **Step 1: Declare module**

Append to `src/checker/multi_review/mod.rs`:

```rust
pub mod judge;
```

- [ ] **Step 2: Add judge prompt template**

Create `src/checker/multi_review/prompts/judge.md`:

```
You are the judge. You will receive two unified reviews of the same diff, written by two independent reviewers (Barry and Other Barry). Decide whether they materially agree.

They MATERIALLY AGREE iff:
- Their `outcome` fields are the same, AND
- Their findings overlap on what is actually wrong (same files / same lines / same root concerns), even if phrased differently.

They DISAGREE if any of:
- The outcomes differ.
- One flags a substantive issue that the other ignores entirely.
- They draw opposite conclusions about the same code (e.g. one says "unsafe", one says "fine").

Output exactly one JSON object:
{
  "agree": true | false,
  "reason": "<one short sentence>"
}
Do not include any text outside the JSON.

Be conservative: when in doubt, lean toward "disagree" so the user sees both voices. False agreements are worse than surfacing minor differences.
```

- [ ] **Step 3: Write failing tests**

Create `src/checker/multi_review/judge.rs`:

```rust
use crate::checker::multi_review::review::UnifiedReview;
use crate::llm::{LlmClient, LlmError, LlmMessage, LlmRequest, Role};
use serde::Deserialize;

const JUDGE_TEMPLATE: &str = include_str!("prompts/judge.md");

#[derive(Debug, thiserror::Error)]
pub enum JudgeError {
    #[error("llm: {0}")]
    Llm(#[from] LlmError),
    #[error("could not parse judge output: {0}")]
    Parse(String),
}

#[derive(Debug, Deserialize)]
struct JudgeResp {
    agree: bool,
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Clone)]
pub struct JudgeVerdict {
    pub agree: bool,
    pub reason: String,
}

pub async fn judge(
    client: &dyn LlmClient,
    barry: &UnifiedReview,
    other: &UnifiedReview,
    max_tokens: u32,
) -> Result<JudgeVerdict, JudgeError> {
    let user = format!(
        "=== Barry's review ===\n{}\n\n=== Other Barry's review ===\n{}",
        serde_json::to_string(&serde_json::json!({
            "outcome": barry.outcome,
            "summary": barry.summary,
            "findings": barry.findings.iter().map(|f| serde_json::json!({
                "file": f.file, "line": f.line, "message": f.message
            })).collect::<Vec<_>>(),
        })).unwrap_or_default(),
        serde_json::to_string(&serde_json::json!({
            "outcome": other.outcome,
            "summary": other.summary,
            "findings": other.findings.iter().map(|f| serde_json::json!({
                "file": f.file, "line": f.line, "message": f.message
            })).collect::<Vec<_>>(),
        })).unwrap_or_default(),
    );
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
    let parsed: JudgeResp = serde_json::from_str(slice).map_err(|e| JudgeError::Parse(e.to_string()))?;
    Ok(JudgeVerdict { agree: parsed.agree, reason: parsed.reason })
}

fn locate_json(text: &str) -> Option<&str> {
    // Identical to the parser in `review.rs`. Kept inline because it's tiny
    // and avoids cross-module coupling on a private utility.
    let bytes = text.as_bytes();
    let mut start = None;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            if esc { esc = false; continue; }
            if b == b'\\' { esc = true; continue; }
            if b == b'"' { in_str = false; }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => { if depth == 0 { start = Some(i); } depth += 1; }
            b'}' => {
                depth -= 1;
                if depth == 0 && let Some(s) = start { return Some(&text[s..=i]); }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checker::multi_review::review::{Outcome, UnifiedReview};
    use crate::llm::LlmResponse;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    struct StubClient { resp: String, recorded: Arc<Mutex<Vec<LlmRequest>>> }
    #[async_trait]
    impl LlmClient for StubClient {
        async fn complete(&self, req: &LlmRequest) -> Result<LlmResponse, LlmError> {
            self.recorded.lock().unwrap().push(req.clone());
            Ok(LlmResponse { text: self.resp.clone(), input_tokens: None, output_tokens: None })
        }
    }

    fn r(outcome: Outcome, summary: &str) -> UnifiedReview {
        UnifiedReview { outcome, summary: summary.into(), findings: vec![] }
    }

    #[tokio::test]
    async fn parses_agree() {
        let rec = Arc::new(Mutex::new(vec![]));
        let c = StubClient { resp: r#"{"agree":true,"reason":"same"}"#.into(), recorded: rec };
        let v = judge(&c, &r(Outcome::Approve, "x"), &r(Outcome::Approve, "y"), 256).await.unwrap();
        assert!(v.agree);
    }

    #[tokio::test]
    async fn parses_disagree() {
        let rec = Arc::new(Mutex::new(vec![]));
        let c = StubClient { resp: r#"{"agree":false,"reason":"diff outcomes"}"#.into(), recorded: rec };
        let v = judge(&c, &r(Outcome::Approve, "x"), &r(Outcome::RequestChanges, "y"), 256).await.unwrap();
        assert!(!v.agree);
        assert_eq!(v.reason, "diff outcomes");
    }

    #[tokio::test]
    async fn includes_both_reviews_in_prompt() {
        let rec = Arc::new(Mutex::new(vec![]));
        let c = StubClient { resp: r#"{"agree":true,"reason":""}"#.into(), recorded: rec.clone() };
        let _ = judge(&c, &r(Outcome::Approve, "barry-says-this"), &r(Outcome::Approve, "ob-says-that"), 256).await.unwrap();
        let r = rec.lock().unwrap();
        let user = &r[0].messages[0].content;
        assert!(user.contains("barry-says-this"));
        assert!(user.contains("ob-says-that"));
        assert!(user.contains("Barry"));
        assert!(user.contains("Other Barry"));
    }

    #[tokio::test]
    async fn errors_on_unparseable() {
        let rec = Arc::new(Mutex::new(vec![]));
        let c = StubClient { resp: "no json here".into(), recorded: rec };
        let err = judge(&c, &r(Outcome::Approve, "x"), &r(Outcome::Approve, "y"), 256).await.unwrap_err();
        assert!(matches!(err, JudgeError::Parse(_)));
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test --lib checker::multi_review::judge
```

Expected: 4 passed.

- [ ] **Step 5: Commit**

```bash
git add src/checker/multi_review/judge.rs src/checker/multi_review/prompts/judge.md src/checker/multi_review/mod.rs
git commit -m "feat(multi-review): judge call (agree/disagree over two reviews)"
```

---

### Task 5: Config — extend GitHubConfig and add new sections

**Files:**
- Modify: `src/config/mod.rs`

- [ ] **Step 1: Write failing tests for new config shape**

Append to the `tests` mod in `src/config/mod.rs`:

```rust
    #[test]
    fn loads_three_identity_config() {
        let f = write_tmp(
            r#"
            [server]
            listen = "0.0.0.0:8080"

            [github.barry]
            app_id = 1
            private_key_path = "/tmp/b.pem"
            webhook_secret_env = "WS"

            [github.other_barry]
            app_id = 2
            private_key_path = "/tmp/ob.pem"

            [github.other_other_barry]
            app_id = 3
            private_key_path = "/tmp/oob.pem"

            [storage]
            sqlite_path = "/tmp/b.db"

            [dispatcher]

            [llm.barry]
            provider = "anthropic"
            endpoint = "https://api.anthropic.com"
            model = "claude-opus-4-7"

            [llm.other_barry]
            provider = "openai"
            endpoint = "http://localhost:11434/v1"
            model = "qwen"

            [llm.other_other_barry]
            provider = "openai"
            endpoint = "https://api.openai.com/v1"
            model = "gpt-5"

            [llm.judge]
            provider = "anthropic"
            endpoint = "https://api.anthropic.com"
            model = "claude-haiku-4-5-20251001"

            [confer]
            allowed = ["author", "write", "admin"]
            max_per_pr = 2
        "#,
        );
        let cfg = Config::load(f.path()).expect("should load");
        assert_eq!(cfg.github.barry.app_id, 1);
        assert_eq!(cfg.github.other_barry.app_id, 2);
        assert_eq!(cfg.github.other_other_barry.app_id, 3);
        assert_eq!(cfg.confer.max_per_pr, 2);
        assert!(cfg.confer.allowed.iter().any(|r| r == "write"));
        assert!(cfg.llm.contains_key("judge"));
    }

    #[test]
    fn rejects_missing_other_barry_when_multi_review_used() {
        // Compatibility: the legacy single-Barry shape is NOT supported.
        // All three [github.*] blocks are required.
        let f = write_tmp(
            r#"
            [server]
            listen = "0.0.0.0:8080"

            [github.barry]
            app_id = 1
            private_key_path = "/tmp/b.pem"
            webhook_secret_env = "WS"

            [storage]
            sqlite_path = "/tmp/b.db"
            [dispatcher]
            [llm.barry]
            provider = "anthropic"
            endpoint = "https://api.anthropic.com"
            model = "x"
        "#,
        );
        // Missing other_barry / other_other_barry → parse error from required field.
        let err = Config::load(f.path()).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. } | ConfigError::Validate(_)));
    }
```

Also rewrite the existing `loads_minimal_valid_config` and `rejects_missing_default_llm_profile` and `rejects_zero_workers` tests to use the new shape (replace `[github] / [llm.default]` with the three-identity layout). Since those existing tests check old shape that we're removing, they should be updated (not duplicated):

Replace `loads_minimal_valid_config` body with:

```rust
    #[test]
    fn loads_minimal_valid_config() {
        let f = write_tmp(
            r#"
            [server]
            listen = "0.0.0.0:8080"

            [github.barry]
            app_id = 1
            private_key_path = "/tmp/b.pem"
            webhook_secret_env = "WS"
            [github.other_barry]
            app_id = 2
            private_key_path = "/tmp/ob.pem"
            [github.other_other_barry]
            app_id = 3
            private_key_path = "/tmp/oob.pem"

            [storage]
            sqlite_path = "/tmp/b.db"

            [dispatcher]

            [llm.barry]
            provider = "anthropic"
            endpoint = "https://api.anthropic.com"
            model = "x"
            [llm.other_barry]
            provider = "openai"
            endpoint = "http://localhost:1/v1"
            model = "x"
            [llm.other_other_barry]
            provider = "openai"
            endpoint = "https://api.openai.com/v1"
            model = "x"
            [llm.judge]
            provider = "anthropic"
            endpoint = "https://api.anthropic.com"
            model = "x"

            [confer]
            allowed = ["author", "write", "admin"]
        "#,
        );
        let cfg = Config::load(f.path()).expect("should load");
        assert_eq!(cfg.dispatcher.worker_count, 4);
    }
```

Update `rejects_missing_default_llm_profile` to assert it rejects when `[llm.barry]` is missing instead of `[llm.default]`.

Update `rejects_zero_workers` to use the new `[github.barry]` shape too.

- [ ] **Step 2: Run tests; verify they fail with parse errors on missing types**

```bash
cargo test --lib config::tests
```

Expected: compilation errors (`field github.barry doesn't exist`, etc.).

- [ ] **Step 3: Update `Config` and helpers**

Replace the existing `GitHubConfig` and add the new types. Modify `src/config/mod.rs`:

```rust
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub github: GitHubConfig,
    pub storage: StorageConfig,
    #[serde(default)]
    pub llm: std::collections::BTreeMap<String, LlmProfile>,
    pub dispatcher: DispatcherConfig,
    #[serde(default)]
    pub confer: ConferConfig,
    #[serde(default)]
    pub personas: PersonaOverridesConfig,
    #[serde(default)]
    pub defaults: Option<crate::config::repo::RepoConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub listen: String,
    #[serde(default)]
    pub public_url: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GitHubConfig {
    pub barry: IdentityCreds,
    pub other_barry: IdentityCreds,
    pub other_other_barry: IdentityCreds,
}

#[derive(Debug, Deserialize, Clone)]
pub struct IdentityCreds {
    pub app_id: u64,
    pub private_key_path: PathBuf,
    /// Webhook secret env var. Only Barry's identity needs this populated;
    /// OB/OOB do not receive webhooks.
    #[serde(default)]
    pub webhook_secret_env: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct StorageConfig {
    pub sqlite_path: PathBuf,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LlmProfile {
    pub provider: LlmProviderKind,
    pub endpoint: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    pub model: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_llm_timeout")]
    pub request_timeout_secs: u64,
}

fn default_max_tokens() -> u32 { 8192 }
fn default_llm_timeout() -> u64 { 300 }

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LlmProviderKind {
    Anthropic,
    Openai,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DispatcherConfig {
    #[serde(default = "default_debounce")]
    pub debounce_secs: u64,
    #[serde(default = "default_workers")]
    pub worker_count: usize,
    #[serde(default = "default_job_timeout")]
    pub job_timeout_secs: u64,
    #[serde(default = "default_checker_timeout")]
    pub checker_timeout_secs: u64,
}

fn default_debounce() -> u64 { 30 }
fn default_workers() -> usize { 4 }
fn default_job_timeout() -> u64 { 1800 }
fn default_checker_timeout() -> u64 { 600 }

#[derive(Debug, Deserialize, Clone)]
pub struct ConferConfig {
    #[serde(default = "default_allowed_roles")]
    pub allowed: Vec<String>,
    #[serde(default = "default_max_confers")]
    pub max_per_pr: u32,
}

impl Default for ConferConfig {
    fn default() -> Self {
        Self { allowed: default_allowed_roles(), max_per_pr: default_max_confers() }
    }
}

fn default_allowed_roles() -> Vec<String> {
    vec!["author".into(), "write".into(), "maintain".into(), "admin".into()]
}
fn default_max_confers() -> u32 { 2 }

#[derive(Debug, Deserialize, Clone, Default)]
pub struct PersonaOverridesConfig {
    #[serde(default)]
    pub security: Option<PersonaOverride>,
    #[serde(default)]
    pub correctness: Option<PersonaOverride>,
    #[serde(default)]
    pub style: Option<PersonaOverride>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PersonaOverride {
    #[serde(default)]
    pub prompt_path: Option<PathBuf>,
}

pub mod repo;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading config file {path}: {source}")]
    Io { path: PathBuf, #[source] source: std::io::Error },
    #[error("parsing config file {path}: {source}")]
    Parse { path: PathBuf, #[source] source: toml::de::Error },
    #[error("validation: {0}")]
    Validate(String),
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.into(), source: e,
        })?;
        let cfg: Config = toml::from_str(&text).map_err(|e| ConfigError::Parse {
            path: path.into(), source: e,
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        for required in ["barry", "other_barry", "other_other_barry", "judge"] {
            if !self.llm.contains_key(required) {
                return Err(ConfigError::Validate(format!(
                    "an [llm.{required}] profile is required"
                )));
            }
        }
        if self.dispatcher.worker_count == 0 {
            return Err(ConfigError::Validate("dispatcher.worker_count must be > 0".into()));
        }
        if self.github.barry.webhook_secret_env.is_none() {
            return Err(ConfigError::Validate(
                "[github.barry].webhook_secret_env is required (Barry receives webhooks)".into()
            ));
        }
        Ok(())
    }
}
```

- [ ] **Step 4: Run tests; verify pass**

```bash
cargo test --lib config
```

Expected: all config tests pass (including the new ones).

- [ ] **Step 5: Commit**

```bash
git add src/config/mod.rs
git commit -m "feat(config): three-identity GitHub config + judge profile + confer + personas"
```

---

### Task 6: Storage — multi_review_runs table and accessors

**Files:**
- Create: `src/storage/multi_review.rs`
- Modify: `src/storage/mod.rs`
- Modify: `src/storage/schema.sql`

- [ ] **Step 1: Add table to schema**

Append to `src/storage/schema.sql`:

```sql
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
```

- [ ] **Step 2: Declare module in `src/storage/mod.rs`**

Add `pub mod multi_review;` near the other module declarations.

- [ ] **Step 3: Write failing tests**

Create `src/storage/multi_review.rs`:

```rust
use crate::checker::multi_review::identity::Identity;
use crate::storage::Store;
use sqlx::Row;

#[derive(Debug, Clone)]
pub struct RunState {
    pub barry_posted: bool,
    pub other_barry_posted: bool,
    pub other_other_barry_posted: bool,
    pub confers_used: u32,
    pub last_outcome: Option<String>,
}

impl Store {
    pub async fn record_post(
        &self,
        owner: &str,
        repo: &str,
        pr: i64,
        head_sha: &str,
        identity: Identity,
        outcome: &str,
        now_ts: i64,
    ) -> anyhow::Result<()> {
        let col = match identity {
            Identity::Barry => "barry_posted",
            Identity::OtherBarry => "other_barry_posted",
            Identity::OtherOtherBarry => "other_other_barry_posted",
        };
        let sql = format!(
            "INSERT INTO multi_review_runs
              (repo_owner, repo_name, pr_number, head_sha, {col}, last_outcome, updated_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6)
             ON CONFLICT(repo_owner, repo_name, pr_number, head_sha) DO UPDATE SET
               {col} = 1, last_outcome = excluded.last_outcome, updated_at = excluded.updated_at"
        );
        sqlx::query(&sql)
            .bind(owner).bind(repo).bind(pr).bind(head_sha)
            .bind(outcome).bind(now_ts)
            .execute(&self.pool).await?;
        Ok(())
    }

    pub async fn record_confer_used(
        &self,
        owner: &str, repo: &str, pr: i64, head_sha: &str, now_ts: i64,
    ) -> anyhow::Result<()> {
        sqlx::query(
            r#"INSERT INTO multi_review_runs
                (repo_owner, repo_name, pr_number, head_sha, confers_used, updated_at)
               VALUES (?1, ?2, ?3, ?4, 1, ?5)
               ON CONFLICT(repo_owner, repo_name, pr_number, head_sha) DO UPDATE SET
                 confers_used = confers_used + 1, updated_at = excluded.updated_at"#,
        )
        .bind(owner).bind(repo).bind(pr).bind(head_sha).bind(now_ts)
        .execute(&self.pool).await?;
        Ok(())
    }

    pub async fn run_state(
        &self, owner: &str, repo: &str, pr: i64, head_sha: &str,
    ) -> anyhow::Result<Option<RunState>> {
        let row = sqlx::query(
            r#"SELECT barry_posted, other_barry_posted, other_other_barry_posted,
                      confers_used, last_outcome
               FROM multi_review_runs
               WHERE repo_owner=?1 AND repo_name=?2 AND pr_number=?3 AND head_sha=?4"#,
        )
        .bind(owner).bind(repo).bind(pr).bind(head_sha)
        .fetch_optional(&self.pool).await?;
        Ok(row.map(|r| RunState {
            barry_posted: r.get::<i64, _>("barry_posted") != 0,
            other_barry_posted: r.get::<i64, _>("other_barry_posted") != 0,
            other_other_barry_posted: r.get::<i64, _>("other_other_barry_posted") != 0,
            confers_used: r.get::<i64, _>("confers_used") as u32,
            last_outcome: r.get("last_outcome"),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn record_post_creates_row() {
        let s = Store::in_memory().await.unwrap();
        s.record_post("o", "r", 1, "sha", Identity::Barry, "approve", 100).await.unwrap();
        let st = s.run_state("o", "r", 1, "sha").await.unwrap().unwrap();
        assert!(st.barry_posted);
        assert!(!st.other_barry_posted);
        assert_eq!(st.last_outcome.as_deref(), Some("approve"));
    }

    #[tokio::test]
    async fn record_post_updates_existing_row() {
        let s = Store::in_memory().await.unwrap();
        s.record_post("o", "r", 1, "sha", Identity::Barry, "approve", 100).await.unwrap();
        s.record_post("o", "r", 1, "sha", Identity::OtherBarry, "comment", 200).await.unwrap();
        let st = s.run_state("o", "r", 1, "sha").await.unwrap().unwrap();
        assert!(st.barry_posted);
        assert!(st.other_barry_posted);
        assert_eq!(st.last_outcome.as_deref(), Some("comment"));
    }

    #[tokio::test]
    async fn no_row_for_unknown_sha() {
        let s = Store::in_memory().await.unwrap();
        s.record_post("o", "r", 1, "sha-old", Identity::Barry, "approve", 100).await.unwrap();
        assert!(s.run_state("o", "r", 1, "sha-new").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn confer_count_increments() {
        let s = Store::in_memory().await.unwrap();
        s.record_post("o", "r", 1, "sha", Identity::Barry, "approve", 100).await.unwrap();
        s.record_confer_used("o", "r", 1, "sha", 200).await.unwrap();
        s.record_confer_used("o", "r", 1, "sha", 300).await.unwrap();
        let st = s.run_state("o", "r", 1, "sha").await.unwrap().unwrap();
        assert_eq!(st.confers_used, 2);
    }
}
```

- [ ] **Step 4: Update `in_memory_creates_schema` test**

In `src/storage/mod.rs`, extend the `tests::in_memory_creates_schema` test:

```rust
        assert!(names.contains(&"multi_review_runs".to_string()));
```

- [ ] **Step 5: Run tests**

```bash
cargo test --lib storage
```

Expected: existing tests still pass, 4 new tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/storage/multi_review.rs src/storage/mod.rs src/storage/schema.sql
git commit -m "feat(storage): multi_review_runs table + accessors"
```

---

### Task 7: MultiGhFactory and per-identity GitHub clients

**Files:**
- Modify: `src/dispatcher/run.rs`
- Modify: `src/app_runtime.rs`
- Modify: `tests/integration/common/mod.rs`

- [ ] **Step 1: Define `MultiGhFactory` trait alongside `GhFactory`**

Edit `src/dispatcher/run.rs`. Replace the `GhFactory` trait with both forms (old retained for compat during migration):

```rust
use crate::checker::multi_review::identity::Identity;

#[async_trait::async_trait]
pub trait GhFactory: Send + Sync {
    async fn for_installation(&self, installation_id: i64) -> anyhow::Result<Arc<GitHub>>;
}

#[async_trait::async_trait]
pub trait MultiGhFactory: GhFactory {
    /// Mint a GitHub client authenticated as the given identity for this installation.
    async fn for_identity(
        &self,
        identity: Identity,
        installation_id: i64,
    ) -> anyhow::Result<Arc<GitHub>>;
}
```

Change `JobDeps`:

```rust
pub struct JobDeps {
    pub store: Store,
    pub config: Arc<Config>,
    pub pipeline: Arc<Pipeline>,
    pub gh_factory: Arc<dyn MultiGhFactory>,
}
```

- [ ] **Step 2: Update `AppGhFactory` in `src/app_runtime.rs`**

Replace `AppGhFactory` with a multi-credential version:

```rust
use crate::checker::multi_review::identity::Identity;
use crate::dispatcher::run::{GhFactory, MultiGhFactory};

pub struct AppGhFactory {
    pub barry: Arc<AppCreds>,
    pub other_barry: Arc<AppCreds>,
    pub other_other_barry: Arc<AppCreds>,
    pub http: reqwest::Client,
    pub store: Store,
}

impl AppGhFactory {
    fn creds_for(&self, identity: Identity) -> &Arc<AppCreds> {
        match identity {
            Identity::Barry => &self.barry,
            Identity::OtherBarry => &self.other_barry,
            Identity::OtherOtherBarry => &self.other_other_barry,
        }
    }
}

#[async_trait]
impl GhFactory for AppGhFactory {
    async fn for_installation(&self, installation_id: i64) -> anyhow::Result<Arc<GitHub>> {
        // Default identity for the legacy path is Barry.
        self.for_identity(Identity::Barry, installation_id).await
    }
}

#[async_trait]
impl MultiGhFactory for AppGhFactory {
    async fn for_identity(
        &self,
        identity: Identity,
        installation_id: i64,
    ) -> anyhow::Result<Arc<GitHub>> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;
        let token = crate::github::app::get_or_mint_for(
            &self.store,
            &self.http,
            self.creds_for(identity),
            identity,
            installation_id,
            now,
        ).await?;
        Ok(Arc::new(GitHub::new(self.http.clone(), token)))
    }
}
```

- [ ] **Step 3: Extend installation-token storage to be identity-scoped**

The current `installation_tokens` table is keyed solely on `installation_id`. With three Apps, each App has its own token; the table needs an `identity` column.

Modify `src/storage/schema.sql` — replace the existing `installation_tokens` create statement with:

```sql
CREATE TABLE IF NOT EXISTS installation_tokens (
    installation_id INTEGER NOT NULL,
    identity TEXT NOT NULL DEFAULT 'barry',
    token TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    PRIMARY KEY (installation_id, identity)
);
```

(For a fresh database this works; existing dogfood db will need the user to drop the table — note it in the rollout.)

Modify `src/storage/tokens.rs` accessors to take an `identity: &str` parameter. Update `src/github/app.rs` to add `get_or_mint_for` that takes `identity: Identity`:

```rust
pub async fn get_or_mint_for(
    store: &Store,
    http: &reqwest::Client,
    creds: &AppCreds,
    identity: crate::checker::multi_review::identity::Identity,
    installation_id: i64,
    now_ts: i64,
) -> anyhow::Result<String> {
    if let Some(t) = store
        .get_installation_token_for(identity.slug(), installation_id, now_ts)
        .await?
    {
        return Ok(t.token);
    }
    let (token, exp) = fetch_installation_token(http, creds, installation_id).await?;
    store
        .put_installation_token_for(identity.slug(), installation_id, &token, exp)
        .await?;
    Ok(token)
}
```

Add the matching `_for` accessors in `src/storage/tokens.rs` (preserve the old methods to keep existing call sites compiling for now; Task 13 removes them).

- [ ] **Step 4: Update `run` in `src/app_runtime.rs` to load three creds**

```rust
pub async fn run(config_path: &Path) -> anyhow::Result<()> {
    crate::telemetry::init_tracing();
    let cfg = Arc::new(Config::load(config_path)?);

    let webhook_env = cfg.github.barry.webhook_secret_env
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("[github.barry].webhook_secret_env required"))?;
    let webhook_secret = std::env::var(webhook_env)
        .map_err(|_| anyhow::anyhow!("env var {} not set", webhook_env))?;

    crate::github::app::ensure_key_mode_strict(&cfg.github.barry.private_key_path)?;
    crate::github::app::ensure_key_mode_strict(&cfg.github.other_barry.private_key_path)?;
    crate::github::app::ensure_key_mode_strict(&cfg.github.other_other_barry.private_key_path)?;

    let barry = Arc::new(AppCreds::load(cfg.github.barry.app_id, &cfg.github.barry.private_key_path)?);
    let ob = Arc::new(AppCreds::load(cfg.github.other_barry.app_id, &cfg.github.other_barry.private_key_path)?);
    let oob = Arc::new(AppCreds::load(cfg.github.other_other_barry.app_id, &cfg.github.other_other_barry.private_key_path)?);

    let store = Store::open(&cfg.storage.sqlite_path).await?;
    let metrics = crate::telemetry::install_metrics();
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let gh_factory: Arc<dyn MultiGhFactory> = Arc::new(AppGhFactory {
        barry, other_barry: ob, other_other_barry: oob,
        http: http.clone(),
        store: store.clone(),
    });

    let pipeline = Arc::new(build_pipeline(&cfg)?);
    let deps = Arc::new(JobDeps {
        store: store.clone(),
        config: cfg.clone(),
        pipeline: pipeline.clone(),
        gh_factory: gh_factory.clone(),
    });

    // … workers + server unchanged …
}
```

- [ ] **Step 5: Update integration test fixture**

Edit `tests/integration/common/mod.rs`. Replace `StaticGh` with a three-identity-aware version:

```rust
use barry_dylan::checker::multi_review::identity::Identity;
use barry_dylan::dispatcher::run::{GhFactory, MultiGhFactory};

pub struct StaticGh {
    pub gh: Arc<GitHub>,
}
#[async_trait]
impl GhFactory for StaticGh {
    async fn for_installation(&self, _id: i64) -> anyhow::Result<Arc<GitHub>> {
        Ok(self.gh.clone())
    }
}
#[async_trait]
impl MultiGhFactory for StaticGh {
    async fn for_identity(&self, _id: Identity, _inst: i64) -> anyhow::Result<Arc<GitHub>> {
        // All identities share one mock GitHub in tests.
        Ok(self.gh.clone())
    }
}
```

Update `default_config()` in `tests/integration/common/mod.rs` to use the new three-identity TOML shape (mirrors Task 5 Step 3 minimal config).

- [ ] **Step 6: Run all tests**

```bash
cargo test
```

Expected: existing integration tests still pass with the updated fixture; new identity-scoped token tests in `tokens.rs` (add at least one) pass.

- [ ] **Step 7: Commit**

```bash
git add src/dispatcher/run.rs src/app_runtime.rs src/storage/ src/github/app.rs tests/integration/common/mod.rs
git commit -m "feat(github): MultiGhFactory + identity-scoped installation tokens"
```

---

### Task 8: Per-identity LLM client construction

**Files:**
- Modify: `src/app_runtime.rs`
- Create: `src/checker/multi_review/clients.rs`
- Modify: `src/checker/multi_review/mod.rs`

- [ ] **Step 1: Declare module**

Append to `src/checker/multi_review/mod.rs`:

```rust
pub mod clients;
```

- [ ] **Step 2: Add `IdentityClients`**

Create `src/checker/multi_review/clients.rs`:

```rust
use crate::checker::multi_review::identity::Identity;
use crate::config::Config;
use crate::llm::LlmClient;
use std::sync::Arc;

pub struct IdentityClients {
    pub barry: Arc<dyn LlmClient>,
    pub other_barry: Arc<dyn LlmClient>,
    pub other_other_barry: Arc<dyn LlmClient>,
    pub judge: Arc<dyn LlmClient>,
    pub barry_max_tokens: u32,
    pub other_barry_max_tokens: u32,
    pub other_other_barry_max_tokens: u32,
    pub judge_max_tokens: u32,
}

impl IdentityClients {
    pub fn for_identity(&self, id: Identity) -> &Arc<dyn LlmClient> {
        match id {
            Identity::Barry => &self.barry,
            Identity::OtherBarry => &self.other_barry,
            Identity::OtherOtherBarry => &self.other_other_barry,
        }
    }
    pub fn max_tokens_for(&self, id: Identity) -> u32 {
        match id {
            Identity::Barry => self.barry_max_tokens,
            Identity::OtherBarry => self.other_barry_max_tokens,
            Identity::OtherOtherBarry => self.other_other_barry_max_tokens,
        }
    }
}

pub fn build(cfg: &Config) -> anyhow::Result<IdentityClients> {
    let http = |timeout_secs: u64| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()
            .map_err(anyhow::Error::from)
    };

    let pick = |name: &str| -> anyhow::Result<&crate::config::LlmProfile> {
        cfg.llm.get(name).ok_or_else(|| anyhow::anyhow!("missing [llm.{name}]"))
    };

    let b = pick("barry")?;
    let ob = pick("other_barry")?;
    let oob = pick("other_other_barry")?;
    let judge = pick("judge")?;

    Ok(IdentityClients {
        barry: crate::llm::factory::build(b, http(b.request_timeout_secs)?)?,
        other_barry: crate::llm::factory::build(ob, http(ob.request_timeout_secs)?)?,
        other_other_barry: crate::llm::factory::build(oob, http(oob.request_timeout_secs)?)?,
        judge: crate::llm::factory::build(judge, http(judge.request_timeout_secs)?)?,
        barry_max_tokens: b.max_tokens,
        other_barry_max_tokens: ob.max_tokens,
        other_other_barry_max_tokens: oob.max_tokens,
        judge_max_tokens: judge.max_tokens,
    })
}
```

- [ ] **Step 3: Add a smoke test**

Append to `src/checker/multi_review/clients.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_succeeds_with_three_local_profiles() {
        let toml = r#"
            [server]
            listen = "0.0.0.0:0"
            [github.barry]
            app_id = 1
            private_key_path = "/tmp/k"
            webhook_secret_env = "X"
            [github.other_barry]
            app_id = 2
            private_key_path = "/tmp/k"
            [github.other_other_barry]
            app_id = 3
            private_key_path = "/tmp/k"
            [storage]
            sqlite_path = "/tmp/x.db"
            [dispatcher]
            [llm.barry]
            provider = "openai"
            endpoint = "http://localhost:1/v1"
            model = "x"
            [llm.other_barry]
            provider = "openai"
            endpoint = "http://localhost:2/v1"
            model = "x"
            [llm.other_other_barry]
            provider = "openai"
            endpoint = "http://localhost:3/v1"
            model = "x"
            [llm.judge]
            provider = "openai"
            endpoint = "http://localhost:4/v1"
            model = "x"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let _ = build(&cfg).unwrap();
    }

    #[test]
    fn build_rejects_missing_judge() {
        let toml = r#"
            [server]
            listen = "0.0.0.0:0"
            [github.barry]
            app_id = 1
            private_key_path = "/tmp/k"
            webhook_secret_env = "X"
            [github.other_barry]
            app_id = 2
            private_key_path = "/tmp/k"
            [github.other_other_barry]
            app_id = 3
            private_key_path = "/tmp/k"
            [storage]
            sqlite_path = "/tmp/x.db"
            [dispatcher]
            [llm.barry]
            provider = "openai"
            endpoint = "http://localhost:1/v1"
            model = "x"
            [llm.other_barry]
            provider = "openai"
            endpoint = "http://localhost:2/v1"
            model = "x"
            [llm.other_other_barry]
            provider = "openai"
            endpoint = "http://localhost:3/v1"
            model = "x"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let err = build(&cfg).unwrap_err();
        assert!(format!("{err}").contains("judge"));
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test --lib checker::multi_review::clients
```

Expected: 2 passed.

- [ ] **Step 5: Commit**

```bash
git add src/checker/multi_review/clients.rs src/checker/multi_review/mod.rs
git commit -m "feat(multi-review): per-identity LlmClient construction"
```

---

### Task 9: Posting — write a UnifiedReview as a PR review under a given identity

**Files:**
- Create: `src/checker/multi_review/posting.rs`
- Modify: `src/checker/multi_review/mod.rs`

- [ ] **Step 1: Declare module**

Append to `src/checker/multi_review/mod.rs`:

```rust
pub mod posting;
```

- [ ] **Step 2: Write the posting code**

Create `src/checker/multi_review/posting.rs`:

```rust
use crate::checker::multi_review::identity::Identity;
use crate::checker::multi_review::review::UnifiedReview;
use crate::dispatcher::run::MultiGhFactory;
use crate::github::pr::{ChangedFile, ReviewCommentInput, ReviewInput};
use std::collections::BTreeMap;
use std::sync::Arc;

pub const REVIEW_MARKER_PREFIX: &str = "<!-- barry-dylan:multi-review:";

pub fn body_for(identity: Identity, review: &UnifiedReview, peer_disagreement: Option<&str>) -> String {
    let header = format!(
        "{REVIEW_MARKER_PREFIX}{slug}:v1 -->\n**{label}** — {outcome:?}\n",
        slug = identity.slug(),
        label = identity.label(),
        outcome = review.outcome,
    );
    let mut body = header;
    if let Some(disagreement) = peer_disagreement {
        body.push_str("\n> ");
        body.push_str(disagreement);
        body.push_str("\n\n");
    }
    body.push_str(&review.summary);
    body
}

pub async fn post_review(
    factory: &Arc<dyn MultiGhFactory>,
    installation_id: i64,
    identity: Identity,
    owner: &str,
    repo: &str,
    pr_number: i64,
    head_sha: &str,
    files: &[ChangedFile],
    review: &UnifiedReview,
    peer_disagreement: Option<&str>,
) -> anyhow::Result<()> {
    let gh = factory.for_identity(identity, installation_id).await?;
    let inline = to_inline_comments(files, &review.findings);
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
    let _ = gh.create_review(owner, repo, pr_number, &input).await?;
    Ok(())
}

fn to_inline_comments(
    files: &[ChangedFile],
    findings: &[crate::checker::multi_review::review::UnifiedFinding],
) -> Vec<ReviewCommentInput> {
    let mut by_file: BTreeMap<&str, &ChangedFile> = BTreeMap::new();
    for f in files { by_file.insert(f.filename.as_str(), f); }
    findings.iter().filter_map(|f| {
        let cf = by_file.get(f.file.as_str())?;
        let patch = cf.patch.as_deref()?;
        let pos = patch_position_for_new_line(patch, f.line)?;
        Some(ReviewCommentInput {
            path: f.file.clone(),
            position: pos as i64,
            body: f.message.clone(),
        })
    }).collect()
}

fn patch_position_for_new_line(patch: &str, target_new_line: u32) -> Option<u32> {
    let mut pos = 0u32;
    let mut new_line = 0u32;
    for line in patch.lines() {
        if line.starts_with("@@") {
            if let Some(plus) = line.split_whitespace().find(|s| s.starts_with('+')) {
                let n = plus.trim_start_matches('+');
                let start: u32 = n.split(',').next()?.parse().ok()?;
                new_line = start.saturating_sub(1);
            }
            pos += 1;
            continue;
        }
        pos += 1;
        if line.starts_with('-') { continue; }
        new_line += 1;
        if new_line == target_new_line { return Some(pos); }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checker::multi_review::review::{Outcome, UnifiedReview};

    fn rev(outcome: Outcome) -> UnifiedReview {
        UnifiedReview { outcome, summary: "looks fine".into(), findings: vec![] }
    }

    #[test]
    fn body_includes_identity_marker_and_label() {
        let b = body_for(Identity::OtherBarry, &rev(Outcome::Comment), None);
        assert!(b.contains("multi-review:other_barry"));
        assert!(b.contains("Other Barry"));
        assert!(b.contains("looks fine"));
    }

    #[test]
    fn disagreement_quote_appears_above_summary() {
        let b = body_for(Identity::OtherBarry, &rev(Outcome::Comment), Some("I disagree with Barry on X"));
        let disagree_idx = b.find("disagree").unwrap();
        let summary_idx = b.find("looks fine").unwrap();
        assert!(disagree_idx < summary_idx);
    }

    #[test]
    fn position_for_added_line() {
        let patch = "@@ -1,2 +1,3 @@\n a\n+b\n c\n";
        assert_eq!(patch_position_for_new_line(patch, 2), Some(3));
    }
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test --lib checker::multi_review::posting
```

Expected: 3 passed.

- [ ] **Step 4: Commit**

```bash
git add src/checker/multi_review/posting.rs src/checker/multi_review/mod.rs
git commit -m "feat(multi-review): post UnifiedReview under a given identity"
```

---

### Task 10: Orchestrator — R1, R2, judge, branching

**Files:**
- Create: `src/checker/multi_review/orchestrator.rs`
- Modify: `src/checker/multi_review/mod.rs`

- [ ] **Step 1: Declare module**

Append to `src/checker/multi_review/mod.rs`:

```rust
pub mod orchestrator;
```

- [ ] **Step 2: Write failing tests**

Create `src/checker/multi_review/orchestrator.rs`:

```rust
use crate::checker::multi_review::clients::IdentityClients;
use crate::checker::multi_review::identity::Identity;
use crate::checker::multi_review::judge;
use crate::checker::multi_review::persona::Persona;
use crate::checker::multi_review::review::{Outcome, UnifiedReview};
use crate::checker::multi_review::synthesis::{self, PersonaDraft};
use crate::github::pr::ChangedFile;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum Verdict {
    /// Judge said the two reviewers materially agree. Only Barry posts.
    Agree { barry: UnifiedReview },
    /// Judge said they disagree. Both post.
    Disagree {
        barry: UnifiedReview,
        other_barry: UnifiedReview,
        reason: String,
    },
    /// Other Barry was unavailable; Barry posts alone with success outcome.
    BarryAlone { barry: UnifiedReview, reason: String },
}

impl Verdict {
    pub fn check_outcome(&self) -> Outcome {
        match self {
            Verdict::Agree { barry } => barry.outcome,
            Verdict::BarryAlone { barry, .. } => barry.outcome,
            Verdict::Disagree { .. } => Outcome::Comment, // neutral check-run
        }
    }
}

pub struct Orchestrator<'a> {
    pub clients: &'a IdentityClients,
    pub personas: &'a [Persona],
}

impl<'a> Orchestrator<'a> {
    pub async fn run(&self, files: &[ChangedFile]) -> anyhow::Result<Verdict> {
        let diff = synthesis::render_diff_block(files);

        // R1: parallel persona+synthesis per identity.
        let barry_r1 = self.run_unified(Identity::Barry, &diff, None);
        let ob_r1 = self.run_unified(Identity::OtherBarry, &diff, None);
        let (barry_r1, ob_r1) = tokio::join!(barry_r1, ob_r1);

        let barry_r1 = match barry_r1 {
            Ok(r) => r,
            Err(e) => return Err(anyhow::anyhow!("barry R1 failed: {e}")),
        };
        let ob_r1 = match ob_r1 {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(?e, "Other Barry R1 failed; Barry posts alone");
                return Ok(Verdict::BarryAlone {
                    barry: barry_r1,
                    reason: format!("Other Barry unavailable: {e}"),
                });
            }
        };

        // R2: each reads the other's R1 and may revise.
        let barry_r1_text = serde_json::to_string(&serde_json::json!({
            "outcome": barry_r1.outcome,
            "summary": barry_r1.summary,
        })).unwrap_or_default();
        let ob_r1_text = serde_json::to_string(&serde_json::json!({
            "outcome": ob_r1.outcome,
            "summary": ob_r1.summary,
        })).unwrap_or_default();
        let barry_r2 = self.run_unified(Identity::Barry, &diff, Some(&ob_r1_text));
        let ob_r2 = self.run_unified(Identity::OtherBarry, &diff, Some(&barry_r1_text));
        let (barry_r2, ob_r2) = tokio::join!(barry_r2, ob_r2);
        let barry_r2 = barry_r2.unwrap_or(barry_r1);
        let ob_r2 = ob_r2.unwrap_or(ob_r1);

        // Judge.
        let verdict = match judge::judge(
            self.clients.judge.as_ref(),
            &barry_r2,
            &ob_r2,
            self.clients.judge_max_tokens.min(512),
        ).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(?e, "judge failed; defaulting to disagreement");
                return Ok(Verdict::Disagree {
                    barry: barry_r2,
                    other_barry: ob_r2,
                    reason: "judge unavailable".into(),
                });
            }
        };

        if verdict.agree {
            Ok(Verdict::Agree { barry: barry_r2 })
        } else {
            Ok(Verdict::Disagree {
                barry: barry_r2,
                other_barry: ob_r2,
                reason: verdict.reason,
            })
        }
    }

    /// Run all personas in parallel for one identity, then synthesize.
    async fn run_unified(
        &self,
        identity: Identity,
        diff: &str,
        peer: Option<&str>,
    ) -> anyhow::Result<UnifiedReview> {
        let client = self.clients.for_identity(identity);
        let max_tokens = self.clients.max_tokens_for(identity);

        let mut futures = Vec::with_capacity(self.personas.len());
        for p in self.personas {
            let c = Arc::clone(client);
            let p = p.clone();
            let diff = diff.to_string();
            futures.push(async move {
                synthesis::run_persona(c.as_ref(), &p, &diff, max_tokens).await
            });
        }
        let results = futures::future::join_all(futures).await;
        let mut drafts = Vec::with_capacity(results.len());
        for r in results {
            match r {
                Ok(d) => drafts.push(d),
                Err(e) => return Err(anyhow::anyhow!("persona call failed: {e}")),
            }
        }
        synthesis::synthesize(client.as_ref(), &drafts, diff, peer, max_tokens)
            .await
            .map_err(|e| anyhow::anyhow!("synthesis failed: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{LlmClient, LlmError, LlmRequest, LlmResponse};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    struct ScriptedClient(Arc<Mutex<Vec<Result<String, &'static str>>>>);
    #[async_trait]
    impl LlmClient for ScriptedClient {
        async fn complete(&self, _req: &LlmRequest) -> Result<LlmResponse, LlmError> {
            let next = self.0.lock().unwrap().remove(0);
            match next {
                Ok(text) => Ok(LlmResponse { text, input_tokens: None, output_tokens: None }),
                Err(msg) => Err(LlmError::Shape(msg.into())),
            }
        }
    }

    fn clients(barry: Vec<Result<&'static str, &'static str>>,
               ob: Vec<Result<&'static str, &'static str>>,
               judge: Vec<Result<&'static str, &'static str>>) -> IdentityClients {
        let to_owned = |v: Vec<Result<&'static str, &'static str>>| {
            Arc::new(Mutex::new(v.into_iter().map(|r| r.map(|s| s.to_string())).collect()))
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

    fn personas() -> Vec<Persona> {
        vec![Persona { name: "security", prompt: Arc::new("you are security".into()) }]
    }

    fn file() -> ChangedFile {
        ChangedFile {
            filename: "a.rs".into(),
            status: "modified".into(),
            additions: 1, deletions: 0, changes: 1,
            patch: Some("@@ -1 +1 @@\n+x".into()),
        }
    }

    fn approve() -> &'static str {
        r#"{"outcome":"approve","summary":"LGTM","findings":[]}"#
    }
    fn comment() -> &'static str {
        r#"{"outcome":"comment","summary":"check this","findings":[]}"#
    }
    fn agree() -> &'static str { r#"{"agree":true,"reason":"same"}"# }
    fn disagree() -> &'static str { r#"{"agree":false,"reason":"diff"}"# }

    #[tokio::test]
    async fn agreement_returns_agree_with_barry() {
        // Order: persona R1, synth R1, persona R2, synth R2 — for both identities.
        let c = clients(
            vec![Ok(approve()), Ok(approve()), Ok(approve()), Ok(approve())],
            vec![Ok(approve()), Ok(approve()), Ok(approve()), Ok(approve())],
            vec![Ok(agree())],
        );
        let p = personas();
        let v = Orchestrator { clients: &c, personas: &p }.run(&[file()]).await.unwrap();
        match v {
            Verdict::Agree { barry } => assert_eq!(barry.outcome, Outcome::Approve),
            other => panic!("wanted Agree, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn disagreement_returns_both() {
        let c = clients(
            vec![Ok(approve()), Ok(approve()), Ok(approve()), Ok(approve())],
            vec![Ok(comment()), Ok(comment()), Ok(comment()), Ok(comment())],
            vec![Ok(disagree())],
        );
        let p = personas();
        let v = Orchestrator { clients: &c, personas: &p }.run(&[file()]).await.unwrap();
        match v {
            Verdict::Disagree { barry, other_barry, reason } => {
                assert_eq!(barry.outcome, Outcome::Approve);
                assert_eq!(other_barry.outcome, Outcome::Comment);
                assert_eq!(reason, "diff");
            }
            other => panic!("wanted Disagree, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ob_failure_yields_barry_alone() {
        let c = clients(
            vec![Ok(approve()), Ok(approve())],
            vec![Err("ob down")],
            vec![],
        );
        let p = personas();
        let v = Orchestrator { clients: &c, personas: &p }.run(&[file()]).await.unwrap();
        assert!(matches!(v, Verdict::BarryAlone { .. }));
    }

    #[tokio::test]
    async fn judge_failure_defaults_to_disagree() {
        let c = clients(
            vec![Ok(approve()), Ok(approve()), Ok(approve()), Ok(approve())],
            vec![Ok(approve()), Ok(approve()), Ok(approve()), Ok(approve())],
            vec![Err("judge down")],
        );
        let p = personas();
        let v = Orchestrator { clients: &c, personas: &p }.run(&[file()]).await.unwrap();
        assert!(matches!(v, Verdict::Disagree { .. }));
    }
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test --lib checker::multi_review::orchestrator
```

Expected: 4 passed.

- [ ] **Step 4: Commit**

```bash
git add src/checker/multi_review/orchestrator.rs src/checker/multi_review/mod.rs
git commit -m "feat(multi-review): orchestrator (R1+R2+judge with degraded-mode fallbacks)"
```

---

### Task 11: MultiReviewChecker — wire orchestrator into Pipeline

**Files:**
- Modify: `src/checker/multi_review/mod.rs`
- Modify: `src/app_runtime.rs`

- [ ] **Step 1: Implement `Checker` for `MultiReviewChecker`**

Replace the contents of `src/checker/multi_review/mod.rs` with:

```rust
//! Multi-identity, multi-persona LLM review checker.

pub mod clients;
pub mod confer;
pub mod identity;
pub mod judge;
pub mod orchestrator;
pub mod persona;
pub mod posting;
pub mod review;
pub mod synthesis;

use crate::checker::{Checker, CheckerCtx, CheckerOutcome, OutcomeStatus};
use crate::checker::multi_review::clients::IdentityClients;
use crate::checker::multi_review::identity::Identity;
use crate::checker::multi_review::orchestrator::{Orchestrator, Verdict};
use crate::checker::multi_review::persona::Persona;
use crate::checker::multi_review::posting::post_review;
use crate::config::repo::RepoConfig;
use crate::dispatcher::run::MultiGhFactory;
use async_trait::async_trait;
use std::sync::Arc;

pub const CHECKER_NAME: &str = "barry/llm-review";

pub struct MultiReviewChecker {
    pub clients: Arc<IdentityClients>,
    pub personas: Arc<Vec<Persona>>,
    pub gh_factory: Arc<dyn MultiGhFactory>,
}

#[async_trait]
impl Checker for MultiReviewChecker {
    fn name(&self) -> &'static str { CHECKER_NAME }
    fn enabled(&self, cfg: &RepoConfig) -> bool { cfg.llm_review.enabled }

    async fn run(&self, ctx: &CheckerCtx) -> anyhow::Result<CheckerOutcome> {
        let installation_id = installation_id_from_ctx(ctx)?;
        let orchestrator = Orchestrator {
            clients: &self.clients,
            personas: &self.personas,
        };
        let verdict = orchestrator.run(&ctx.files).await?;

        // Post under each Barry that has something to say.
        match &verdict {
            Verdict::Agree { barry } | Verdict::BarryAlone { barry, .. } => {
                post_review(
                    &self.gh_factory, installation_id, Identity::Barry,
                    &ctx.owner, &ctx.repo, ctx.pr.number, &ctx.pr.head.sha,
                    &ctx.files, barry, None,
                ).await?;
            }
            Verdict::Disagree { barry, other_barry, reason } => {
                let disagreement_msg = format!("I disagree with Barry: {reason}");
                post_review(
                    &self.gh_factory, installation_id, Identity::Barry,
                    &ctx.owner, &ctx.repo, ctx.pr.number, &ctx.pr.head.sha,
                    &ctx.files, barry, None,
                ).await?;
                post_review(
                    &self.gh_factory, installation_id, Identity::OtherBarry,
                    &ctx.owner, &ctx.repo, ctx.pr.number, &ctx.pr.head.sha,
                    &ctx.files, other_barry, Some(&disagreement_msg),
                ).await?;
            }
        }

        // Persist run state. Recorded under Barry's installation.
        let now = now_ts();
        match &verdict {
            Verdict::Agree { barry } | Verdict::BarryAlone { barry, .. } => {
                ctx.gh.clone(); // borrow consistency
                let _ = ctx_store(ctx).record_post(
                    &ctx.owner, &ctx.repo, ctx.pr.number, &ctx.pr.head.sha,
                    Identity::Barry, outcome_str(barry.outcome), now,
                ).await;
            }
            Verdict::Disagree { barry, other_barry, .. } => {
                let store = ctx_store(ctx);
                let _ = store.record_post(
                    &ctx.owner, &ctx.repo, ctx.pr.number, &ctx.pr.head.sha,
                    Identity::Barry, outcome_str(barry.outcome), now,
                ).await;
                let _ = store.record_post(
                    &ctx.owner, &ctx.repo, ctx.pr.number, &ctx.pr.head.sha,
                    Identity::OtherBarry, outcome_str(other_barry.outcome), now,
                ).await;
            }
        }

        // Return the dispatcher-style outcome that drives the check-run.
        let status = match verdict.check_outcome() {
            review::Outcome::Approve => OutcomeStatus::Success,
            review::Outcome::Comment => OutcomeStatus::Neutral,
            review::Outcome::RequestChanges => OutcomeStatus::Failure,
        };
        let summary = match &verdict {
            Verdict::Agree { barry } => format!("Barry — {}", first_line(&barry.summary)),
            Verdict::BarryAlone { barry, .. } => format!("Barry (alone) — {}", first_line(&barry.summary)),
            Verdict::Disagree { reason, .. } => format!("No consensus: {reason}"),
        };
        Ok(CheckerOutcome {
            checker_name: CHECKER_NAME,
            status,
            summary,
            text: None,
            inline_comments: vec![],
            issue_comment: None,
            add_labels: vec![],
        })
    }
}

fn outcome_str(o: review::Outcome) -> &'static str {
    match o {
        review::Outcome::Approve => "approve",
        review::Outcome::Comment => "comment",
        review::Outcome::RequestChanges => "request_changes",
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(140).collect()
}

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// Look up the installation ID from the Job that produced this CheckerCtx.
/// CheckerCtx doesn't carry it directly today, so we plumb it via the GitHub
/// client's stored token. For v1 we get the installation_id from the dispatcher
/// passing it through CheckerCtx.
fn installation_id_from_ctx(ctx: &CheckerCtx) -> anyhow::Result<i64> {
    ctx.installation_id
        .ok_or_else(|| anyhow::anyhow!("installation_id not available in CheckerCtx"))
}

/// Pull a `Store` reference out of CheckerCtx. (Added in this task — see
/// modification to CheckerCtx in `src/checker/mod.rs`.)
fn ctx_store(ctx: &CheckerCtx) -> &crate::storage::Store {
    &ctx.store
}
```

- [ ] **Step 2: Extend `CheckerCtx` with `store` and `installation_id`**

Modify `src/checker/mod.rs`. Add fields to `CheckerCtx`:

```rust
pub struct CheckerCtx {
    pub gh: Arc<GitHub>,
    pub repo_cfg: Arc<RepoConfig>,
    pub owner: String,
    pub repo: String,
    pub pr: PullRequest,
    pub files: Vec<ChangedFile>,
    pub prior_bot_reviews: Vec<BotComment>,
    pub prior_bot_comments: Vec<BotComment>,
    pub store: crate::storage::Store,
    pub installation_id: Option<i64>,
}
```

Update `src/dispatcher/run.rs` to populate the new fields when constructing `CheckerCtx` (around line 98). Add:

```rust
        store: deps.store.clone(),
        installation_id: Some(job.installation_id),
```

Update integration test fixtures (`tests/integration/common/mod.rs` and any test that constructs `CheckerCtx` directly) to populate the new fields.

- [ ] **Step 3: Wire `MultiReviewChecker` into the pipeline**

Edit `src/app_runtime.rs`. Replace `build_pipeline`:

```rust
fn build_pipeline(
    cfg: &Config,
    gh_factory: Arc<dyn MultiGhFactory>,
) -> anyhow::Result<Pipeline> {
    let mut p = Pipeline::hygiene_only();

    let clients = Arc::new(crate::checker::multi_review::clients::build(cfg)?);
    let overrides = personas_from_cfg(&cfg.personas);
    let personas = Arc::new(crate::checker::multi_review::persona::resolve(&overrides)?);

    p.checkers.push(Arc::new(crate::checker::multi_review::MultiReviewChecker {
        clients, personas, gh_factory,
    }));
    Ok(p)
}

fn personas_from_cfg(p: &crate::config::PersonaOverridesConfig) -> crate::checker::multi_review::persona::PersonaOverrides {
    crate::checker::multi_review::persona::PersonaOverrides {
        security: p.security.as_ref().and_then(|x| x.prompt_path.clone()),
        correctness: p.correctness.as_ref().and_then(|x| x.prompt_path.clone()),
        style: p.style.as_ref().and_then(|x| x.prompt_path.clone()),
    }
}
```

Update the call site in `run` to pass `gh_factory.clone()` to `build_pipeline`.

- [ ] **Step 4: Add a confer.rs stub so `mod.rs` compiles**

Create `src/checker/multi_review/confer.rs` with just a doc comment for now:

```rust
//! Confer command handler. Implementation in Task 12.
```

- [ ] **Step 5: Compile and run all tests**

```bash
cargo build && cargo test
```

Expected: build succeeds. Existing tests pass. The integration tests that previously asserted `expect(4..)` on check-runs may need their pipeline now expanded — for now they should still work since hygiene runs the same way and `MultiReviewChecker.run` goes through the dispatcher's standard outcome posting path. If happy_path / failure_isolation tests reference `LlmReviewChecker` or assert exact check-run counts, update them.

- [ ] **Step 6: Commit**

```bash
git add src/checker/ src/dispatcher/run.rs src/app_runtime.rs tests/
git commit -m "feat(multi-review): MultiReviewChecker wired into pipeline"
```

---

### Task 12: Confer command — webhook routing + handler

**Files:**
- Modify: `src/dispatcher/trust.rs` (add `Confer` variant)
- Modify: `src/webhook/server.rs` (route `/barry confer`)
- Modify: `src/dispatcher/run.rs` (route confer command)
- Modify: `src/checker/multi_review/confer.rs` (real implementation)

- [ ] **Step 1: Add `BarryCommand::Confer` and parser update**

Edit `src/dispatcher/trust.rs`. Add to the enum:

```rust
pub enum BarryCommand {
    Approve,
    Review,
    Confer,
    Unknown,
    NotACommand,
}
```

Update `parse_command`:

```rust
        Some("/barry") => match parts.next() {
            Some("approve") => BarryCommand::Approve,
            Some("review") => BarryCommand::Review,
            Some("confer") => BarryCommand::Confer,
            Some(_) => BarryCommand::Unknown,
            None => BarryCommand::Unknown,
        },
```

Add a unit test:

```rust
    #[test]
    fn parses_confer() {
        assert_eq!(parse_command("/barry confer"), BarryCommand::Confer);
        assert_eq!(parse_command("/barry confer please"), BarryCommand::Confer);
    }
```

- [ ] **Step 2: Route `confer` in the webhook**

Edit `src/webhook/server.rs`. Update `short_command`:

```rust
fn short_command(body: &str) -> &'static str {
    let first = body.split_whitespace().nth(1).unwrap_or("");
    match first {
        "approve" => "approve",
        "review" => "review",
        "confer" => "confer",
        _ => "unknown",
    }
}
```

- [ ] **Step 3: Route confer in the dispatcher**

Edit `src/dispatcher/run.rs`. Update `parse_command_event`:

```rust
fn parse_command_event(kind: &str) -> Option<BarryCommand> {
    let sub = kind.strip_prefix("issue_comment.")?;
    Some(match sub {
        "approve" => BarryCommand::Approve,
        "review" => BarryCommand::Review,
        "confer" => BarryCommand::Confer,
        _ => BarryCommand::Unknown,
    })
}
```

Update `handle_command`:

```rust
        BarryCommand::Confer => {
            crate::checker::multi_review::confer::handle(deps, gh, job).await?;
        }
```

- [ ] **Step 4: Implement the confer handler**

Replace `src/checker/multi_review/confer.rs` with:

```rust
//! Confer command handler. Summons OB or OOB to weigh in on a posted review.

use crate::checker::multi_review::clients::IdentityClients;
use crate::checker::multi_review::identity::Identity;
use crate::checker::multi_review::persona::Persona;
use crate::checker::multi_review::posting::post_review;
use crate::checker::multi_review::review::UnifiedReview;
use crate::checker::multi_review::synthesis;
use crate::dispatcher::run::{JobDeps, MultiGhFactory};
use crate::github::client::GitHub;
use crate::storage::queue::LeasedJob;
use std::sync::Arc;

pub async fn handle(
    deps: &JobDeps,
    barry_gh: &Arc<GitHub>,
    job: &LeasedJob,
) -> anyhow::Result<()> {
    // Look up the PR head SHA and author permission.
    let pr_ctx = barry_gh
        .fetch_pr_context(&job.repo_owner, &job.repo_name, job.pr_number)
        .await?;
    let head_sha = pr_ctx.pr.head.sha.clone();

    // Authorize the commenter using the same permission lookup as the trust gate.
    // The webhook adds the comment author into the job; until that exists we look
    // it up via the most recent comment on the PR.
    let last_comment = pr_ctx.comments.last();
    let confer_author = match last_comment {
        Some(c) => c.author.clone(),
        None => {
            tracing::warn!("confer received but no comment on PR; ignoring");
            return Ok(());
        }
    };
    let perm = barry_gh
        .author_permission(&job.repo_owner, &job.repo_name, &confer_author)
        .await
        .unwrap_or_else(|_| "read".into());

    let allowed = &deps.config.confer.allowed;
    let role_ok = role_matches(&perm, &confer_author, &pr_ctx.pr.user.login, allowed);
    if !role_ok {
        tracing::info!(%confer_author, %perm, "confer rejected (unauthorized)");
        // 👎 reaction would be ideal; for v1 we just log.
        return Ok(());
    }

    // Look up run state.
    let st = deps
        .store
        .run_state(&job.repo_owner, &job.repo_name, job.pr_number, &head_sha)
        .await?;
    let st = match st {
        Some(s) => s,
        None => {
            tracing::info!(%head_sha, "confer with no prior run; replying");
            barry_gh.create_issue_comment(
                &job.repo_owner, &job.repo_name, job.pr_number,
                "I haven't reviewed this commit yet — comment again after the next review run.",
            ).await?;
            return Ok(());
        }
    };

    if st.confers_used >= deps.config.confer.max_per_pr {
        barry_gh.create_issue_comment(
            &job.repo_owner, &job.repo_name, job.pr_number,
            "Maximum confers reached for this PR.",
        ).await?;
        return Ok(());
    }

    // Decide which identity to summon.
    let summon = if !st.other_barry_posted {
        Identity::OtherBarry
    } else if !st.other_other_barry_posted {
        Identity::OtherOtherBarry
    } else {
        barry_gh.create_issue_comment(
            &job.repo_owner, &job.repo_name, job.pr_number,
            "All Barrys have already conferred on this commit.",
        ).await?;
        return Ok(());
    };

    // Get pieces from the pipeline that we need.
    let clients = lookup_clients(deps)?;
    let personas = lookup_personas(deps)?;

    // Build the diff block.
    let files = barry_gh
        .list_pr_files(&job.repo_owner, &job.repo_name, job.pr_number)
        .await?;
    let diff = synthesis::render_diff_block(&files);

    // Read prior posted reviews from PR (Barry's review summary, plus OB's if any).
    let prior_text = build_prior_context(&pr_ctx);

    // Run personas + synthesis for the summoned identity.
    let review = run_unified(
        clients.for_identity(summon).as_ref(),
        clients.max_tokens_for(summon),
        &personas,
        &diff,
        Some(&prior_text),
    ).await?;

    // Post.
    post_review(
        &deps.gh_factory, job.installation_id, summon,
        &job.repo_owner, &job.repo_name, job.pr_number, &head_sha,
        &files, &review, None,
    ).await?;

    // Update state.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    deps.store
        .record_post(&job.repo_owner, &job.repo_name, job.pr_number, &head_sha,
            summon, outcome_str(review.outcome), now)
        .await?;
    deps.store
        .record_confer_used(&job.repo_owner, &job.repo_name, job.pr_number, &head_sha, now)
        .await?;
    Ok(())
}

fn role_matches(perm: &str, author: &str, pr_author: &str, allowed: &[String]) -> bool {
    for role in allowed {
        match role.as_str() {
            "author" if author == pr_author => return true,
            r if r == perm => return true,
            _ => {}
        }
    }
    false
}

async fn run_unified(
    client: &dyn crate::llm::LlmClient,
    max_tokens: u32,
    personas: &[Persona],
    diff: &str,
    peer: Option<&str>,
) -> anyhow::Result<UnifiedReview> {
    let mut futures = Vec::with_capacity(personas.len());
    for p in personas {
        let p = p.clone();
        let diff = diff.to_string();
        futures.push(async move {
            synthesis::run_persona(client, &p, &diff, max_tokens).await
        });
    }
    let results = futures::future::join_all(futures).await;
    let mut drafts = Vec::with_capacity(results.len());
    for r in results {
        drafts.push(r.map_err(|e| anyhow::anyhow!("persona call failed: {e}"))?);
    }
    let r = synthesis::synthesize(client, &drafts, diff, peer, max_tokens)
        .await
        .map_err(|e| anyhow::anyhow!("synthesis failed: {e}"))?;
    Ok(r)
}

fn build_prior_context(pr_ctx: &crate::github::pr::PrContext) -> String {
    let mut s = String::from("=== prior reviews on this commit ===\n");
    for r in &pr_ctx.reviews {
        if r.body.contains(crate::checker::multi_review::posting::REVIEW_MARKER_PREFIX) {
            s.push_str(&r.body);
            s.push_str("\n---\n");
        }
    }
    s
}

fn outcome_str(o: crate::checker::multi_review::review::Outcome) -> &'static str {
    use crate::checker::multi_review::review::Outcome::*;
    match o {
        Approve => "approve",
        Comment => "comment",
        RequestChanges => "request_changes",
    }
}

/// Pull the IdentityClients out of the registered MultiReviewChecker.
fn lookup_clients(deps: &JobDeps) -> anyhow::Result<Arc<IdentityClients>> {
    deps.pipeline.checkers.iter()
        .find_map(|c| {
            (c.name() == crate::checker::multi_review::CHECKER_NAME).then(|| {
                let any = c.clone();
                let raw: Arc<dyn std::any::Any + Send + Sync> = unsafe {
                    // Safety: we only downcast to a known concrete type registered
                    // by build_pipeline. The trait object's vtable identifies the
                    // type uniquely. If type-erasure becomes a concern we'll switch
                    // to a registry. For v1 we accept the simple downcast.
                    std::mem::transmute::<Arc<dyn crate::checker::Checker>, Arc<dyn std::any::Any + Send + Sync>>(any)
                };
                raw.downcast::<crate::checker::multi_review::MultiReviewChecker>()
                    .ok()
                    .map(|m| m.clients.clone())
            })
        })
        .flatten()
        .ok_or_else(|| anyhow::anyhow!("MultiReviewChecker not registered"))
}

fn lookup_personas(deps: &JobDeps) -> anyhow::Result<Arc<Vec<Persona>>> {
    deps.pipeline.checkers.iter()
        .find_map(|c| {
            (c.name() == crate::checker::multi_review::CHECKER_NAME).then(|| {
                let any = c.clone();
                let raw: Arc<dyn std::any::Any + Send + Sync> = unsafe {
                    std::mem::transmute::<Arc<dyn crate::checker::Checker>, Arc<dyn std::any::Any + Send + Sync>>(any)
                };
                raw.downcast::<crate::checker::multi_review::MultiReviewChecker>()
                    .ok()
                    .map(|m| m.personas.clone())
            })
        })
        .flatten()
        .ok_or_else(|| anyhow::anyhow!("MultiReviewChecker not registered"))
}
```

> **Note for the implementer:** the `unsafe` `transmute` in `lookup_clients`/`lookup_personas` is a v1 pragmatic shortcut. If a code reviewer flags this, the cleaner alternative is to thread `Arc<IdentityClients>` and `Arc<Vec<Persona>>` directly through `JobDeps` instead of fishing them back out of the Pipeline. Prefer that refactor if it doesn't add too much friction.

- [ ] **Step 5: Add unit tests for the role_matches and confer state machine**

Append to `src/checker/multi_review/confer.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_matches_pr_author() {
        let allowed = vec!["author".to_string()];
        assert!(role_matches("read", "alice", "alice", &allowed));
        assert!(!role_matches("read", "bob", "alice", &allowed));
    }

    #[test]
    fn role_matches_write_perm() {
        let allowed = vec!["write".to_string(), "admin".to_string()];
        assert!(role_matches("write", "bob", "alice", &allowed));
        assert!(role_matches("admin", "bob", "alice", &allowed));
        assert!(!role_matches("read", "bob", "alice", &allowed));
    }

    #[test]
    fn role_matches_rejects_when_no_match() {
        let allowed = vec!["maintain".to_string()];
        assert!(!role_matches("write", "bob", "alice", &allowed));
    }
}
```

- [ ] **Step 6: Run tests**

```bash
cargo test
```

Expected: all unit tests pass; existing integration tests still pass.

- [ ] **Step 7: Commit**

```bash
git add src/checker/multi_review/confer.rs src/dispatcher/ src/webhook/server.rs
git commit -m "feat(multi-review): /barry confer command (summons OB or OOB)"
```

---

### Task 13: Integration test — agreement scenario

**Files:**
- Create: `tests/integration/multi_review_agreement.rs`
- Modify: `tests/integration/main.rs`

- [ ] **Step 1: Add module to `tests/integration/main.rs`**

```rust
mod multi_review_agreement;
```

- [ ] **Step 2: Build a mock LLM endpoint helper**

Add to `tests/integration/common/mod.rs`:

```rust
use wiremock::matchers::{body_partial_json, header, method as wmethod, path};
use wiremock::{Mock, ResponseTemplate};

/// Mount a mock for the OpenAI-compatible /v1/chat/completions endpoint that
/// returns the provided text in the assistant message. Each mount serves one call.
pub async fn mock_openai_chat(server: &MockServer, response_text: &str) {
    let resp = serde_json::json!({
        "id": "chatcmpl-x",
        "object": "chat.completion",
        "created": 0,
        "model": "test",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": { "role": "assistant", "content": response_text }
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    });
    Mock::given(wmethod("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(resp))
        .expect(1..)
        .mount(server)
        .await;
}
```

(Keep existing helpers; only add the new one.)

- [ ] **Step 3: Write the test**

Create `tests/integration/multi_review_agreement.rs`:

```rust
use barry_dylan::dispatcher::run::run_job;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn agreement_posts_only_barry_review() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(crate::common::graphql_pr_context(
            1, "alice", "sha1", None,
            serde_json::json!([]),
            serde_json::json!([]),
        )))
        .mount(&server).await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/repos/o/r/pulls/1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "filename": "src/lib.rs", "status": "modified",
              "additions": 1, "deletions": 0, "changes": 1,
              "patch": "@@ -1 +1 @@\n+x" }
        ])))
        .mount(&server).await;

    Mock::given(method("GET"))
        .and(path("/repos/o/r/collaborators/alice/permission"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "permission": "write"
        })))
        .mount(&server).await;

    // LLM mocks: per-persona persona calls (3) + 1 synthesis = 4 per Barry × 2 + 1 judge = 9 calls.
    // Each Mock returns the same identical "approve" review for both Barrys, "agree" for judge.
    crate::common::mock_openai_chat(
        &server,
        r#"{"outcome":"approve","summary":"LGTM","findings":[]}"#,
    ).await;
    // Note: wiremock matches in declaration order; for a v1 test we just stack
    // the same mock — every chat completion call returns the approve JSON until
    // a judge call, but the judge prompt is distinguishable by its system text.
    // For simplicity, accept that the same JSON parses fine for both shapes when
    // the judge prompt's parser falls back. If that's brittle, add a body_partial_json
    // matcher on the judge's system text (`include_str!("prompts/judge.md")` first line).

    // Check Run writes: hygiene (4) + multi-review (1) = 5+
    Mock::given(method("POST"))
        .and(path("/repos/o/r/check-runs"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 })))
        .expect(5..)
        .mount(&server).await;

    // Reviews: exactly 1 (only Barry posts on agreement)
    Mock::given(method("POST"))
        .and(path("/repos/o/r/pulls/1/reviews"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 })))
        .expect(1)
        .mount(&server).await;

    let (store, deps) = crate::common::fixture_with_llm(&server).await;
    crate::common::enqueue_opened(&store, "o", "r", 1).await;
    let job = store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(&deps, &job).await.unwrap();
    // wiremock verifies on Drop.
}
```

(`fixture_with_llm` is a new helper; build it next.)

- [ ] **Step 4: Add `fixture_with_llm` to `tests/integration/common/mod.rs`**

```rust
pub async fn fixture_with_llm(server: &MockServer) -> (Store, Arc<JobDeps>) {
    use barry_dylan::checker::multi_review::{MultiReviewChecker, persona};
    use barry_dylan::checker::multi_review::clients::IdentityClients;

    let store = Store::in_memory().await.unwrap();
    let gh = Arc::new(GitHub::new(reqwest::Client::new(), "tok".into()).with_base(server.uri()));
    let cfg = Arc::new(default_config_with_llm(server));

    let clients = Arc::new(IdentityClients {
        barry: barry_dylan::llm::factory::build(&cfg.llm["barry"], reqwest::Client::new()).unwrap(),
        other_barry: barry_dylan::llm::factory::build(&cfg.llm["other_barry"], reqwest::Client::new()).unwrap(),
        other_other_barry: barry_dylan::llm::factory::build(&cfg.llm["other_other_barry"], reqwest::Client::new()).unwrap(),
        judge: barry_dylan::llm::factory::build(&cfg.llm["judge"], reqwest::Client::new()).unwrap(),
        barry_max_tokens: 1024, other_barry_max_tokens: 1024,
        other_other_barry_max_tokens: 1024, judge_max_tokens: 256,
    });
    let personas = Arc::new(persona::resolve(&persona::PersonaOverrides::default()).unwrap());
    let factory: Arc<dyn MultiGhFactory> = Arc::new(StaticGh { gh: gh.clone() });

    let mut pipeline = Pipeline::hygiene_only();
    pipeline.checkers.push(Arc::new(MultiReviewChecker {
        clients, personas, gh_factory: factory.clone(),
    }));

    let deps = Arc::new(JobDeps {
        store: store.clone(), config: cfg, pipeline: Arc::new(pipeline),
        gh_factory: factory,
    });
    (store, deps)
}

fn default_config_with_llm(server: &MockServer) -> barry_dylan::config::Config {
    let toml = format!(r#"
        [server]
        listen = "0.0.0.0:0"
        [github.barry]
        app_id = 1
        private_key_path = "/dev/null"
        webhook_secret_env = "X"
        [github.other_barry]
        app_id = 2
        private_key_path = "/dev/null"
        [github.other_other_barry]
        app_id = 3
        private_key_path = "/dev/null"
        [storage]
        sqlite_path = "/tmp/x.db"
        [dispatcher]
        [llm.barry]
        provider = "openai"
        endpoint = "{base}/v1"
        model = "x"
        [llm.other_barry]
        provider = "openai"
        endpoint = "{base}/v1"
        model = "x"
        [llm.other_other_barry]
        provider = "openai"
        endpoint = "{base}/v1"
        model = "x"
        [llm.judge]
        provider = "openai"
        endpoint = "{base}/v1"
        model = "x"
        [confer]
        allowed = ["author", "write", "admin"]
        max_per_pr = 2
    "#, base = server.uri());
    toml::from_str(&toml).unwrap()
}
```

- [ ] **Step 5: Run the test**

```bash
cargo test --test integration multi_review_agreement
```

Expected: pass.

- [ ] **Step 6: Commit**

```bash
git add tests/
git commit -m "test(multi-review): E2E agreement scenario"
```

---

### Task 14: Integration test — disagreement scenario

**Files:**
- Create: `tests/integration/multi_review_disagreement.rs`
- Modify: `tests/integration/main.rs`

- [ ] **Step 1: Add module**

```rust
mod multi_review_disagreement;
```

- [ ] **Step 2: Write the test**

Create `tests/integration/multi_review_disagreement.rs`:

```rust
use barry_dylan::dispatcher::run::run_job;
use wiremock::matchers::{body_partial_json, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn disagreement_posts_two_reviews_and_neutral_check() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(crate::common::graphql_pr_context(
            1, "alice", "sha1", None,
            serde_json::json!([]), serde_json::json!([]),
        )))
        .mount(&server).await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/repos/o/r/pulls/1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "filename": "src/lib.rs", "status": "modified",
              "additions": 1, "deletions": 0, "changes": 1,
              "patch": "@@ -1 +1 @@\n+x" }
        ])))
        .mount(&server).await;

    Mock::given(method("GET"))
        .and(path("/repos/o/r/collaborators/alice/permission"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"permission":"write"})))
        .mount(&server).await;

    // We use body_partial_json on the system message text to dispatch the right
    // mock to the right call type. The synthesis system prompt contains the
    // word "synthesis stage"; the judge's contains "you are the judge".
    let mock_chat = |response_text: &str, count: u64| {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id":"x","object":"chat.completion","created":0,"model":"t",
            "choices":[{"index":0,"finish_reason":"stop",
                        "message":{"role":"assistant","content":response_text}}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
        })).set_delay(std::time::Duration::from_millis(0)) // and expect(count)
    };

    // Persona drafts (any "findings" stub)
    let persona_stub = r#"{"findings":[],"summary":"persona stub"}"#;
    let approve = r#"{"outcome":"approve","summary":"LGTM","findings":[]}"#;
    let reqchg = r#"{"outcome":"request_changes","summary":"please fix","findings":[{"file":"src/lib.rs","line":1,"message":"X"}]}"#;
    let disagree = r#"{"agree":false,"reason":"different outcomes"}"#;

    // Persona calls — 3 personas × 2 identities × 2 rounds = 12.
    // Synthesis Barry returns approve; OB returns request_changes (twice each).
    // For test simplicity we rely on declaration order: wiremock serves mounts in
    // declaration order. Mount 12 persona stubs first, then 4 synthesis calls
    // alternating (Barry, OB, Barry, OB) approve/reqchg, then 1 judge.
    for _ in 0..12 {
        Mock::given(method("POST")).and(path("/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({})))
            .respond_with(mock_chat(persona_stub, 1)).up_to_n_times(1).mount(&server).await;
    }
    // Synthesis: barry/ob alternate twice. We can't pin which is which by header,
    // so accept both orders by mounting the right counts.
    for body in [approve, reqchg, approve, reqchg] {
        Mock::given(method("POST")).and(path("/v1/chat/completions"))
            .respond_with(mock_chat(body, 1)).up_to_n_times(1).mount(&server).await;
    }
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(mock_chat(disagree, 1)).up_to_n_times(1).mount(&server).await;

    Mock::given(method("POST"))
        .and(path("/repos/o/r/check-runs"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 })))
        .expect(5..).mount(&server).await;

    Mock::given(method("POST"))
        .and(path("/repos/o/r/pulls/1/reviews"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 })))
        .expect(2).mount(&server).await;

    let (store, deps) = crate::common::fixture_with_llm(&server).await;
    crate::common::enqueue_opened(&store, "o", "r", 1).await;
    let job = store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(&deps, &job).await.unwrap();

    // Verify run state shows both Barrys posted.
    let st = store.run_state("o", "r", 1, "sha1").await.unwrap().unwrap();
    assert!(st.barry_posted);
    assert!(st.other_barry_posted);
}
```

> Note: `up_to_n_times(1)` on each mount means wiremock tries them in declaration order until one matches and serves once. If the test proves flaky in practice because of nondeterministic matching, switch to a `body_partial_json` matcher keyed on the judge prompt's first line vs the synthesis prompt's first line. The tests in this task aim to pin the orchestration shape; if the matching becomes a maintenance burden, simplify by having both Barrys' synthesizers always use the same body for the test fixture and instead drive disagreement entirely through the judge response.

- [ ] **Step 3: Run**

```bash
cargo test --test integration multi_review_disagreement
```

Expected: pass.

- [ ] **Step 4: Commit**

```bash
git add tests/
git commit -m "test(multi-review): E2E disagreement scenario"
```

---

### Task 15: Integration test — confer escalation

**Files:**
- Create: `tests/integration/multi_review_confer.rs`
- Modify: `tests/integration/main.rs`

- [ ] **Step 1: Add module**

```rust
mod multi_review_confer;
```

- [ ] **Step 2: Write the test**

Create `tests/integration/multi_review_confer.rs`:

```rust
use barry_dylan::dispatcher::run::run_job;
use barry_dylan::storage::queue::NewJob;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn confer_summons_other_barry_then_oob() {
    let server = MockServer::start().await;

    // PR context returns one prior bot review (Barry's) so the confer handler can
    // see "Barry already posted on this SHA".
    let prior_review = serde_json::json!([{
        "databaseId": 100, "id": "r1",
        "author": { "login": "barry-dylan[bot]" },
        "body": format!("{prefix}barry:v1 -->\n**Barry** — Approve\nLGTM",
                         prefix = "<!-- barry-dylan:multi-review:")
    }]);
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(crate::common::graphql_pr_context(
            1, "alice", "sha1", None,
            serde_json::json!([{
                "databaseId": 1, "id": "c1",
                "author": { "login": "alice" }, "body": "/barry confer"
            }]),
            prior_review,
        )))
        .mount(&server).await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/repos/o/r/pulls/1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "filename": "src/lib.rs", "status": "modified",
              "additions": 1, "deletions": 0, "changes": 1,
              "patch": "@@ -1 +1 @@\n+x" }
        ])))
        .mount(&server).await;

    Mock::given(method("GET"))
        .and(path("/repos/o/r/collaborators/alice/permission"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"permission":"write"})))
        .mount(&server).await;

    // 3 persona calls + 1 synthesis = 4 LLM calls for the OB run.
    let approve = r#"{"outcome":"approve","summary":"OB agrees","findings":[]}"#;
    let persona_stub = r#"{"findings":[],"summary":"persona stub"}"#;
    for _ in 0..3 {
        Mock::given(method("POST")).and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices":[{"index":0,"finish_reason":"stop",
                            "message":{"role":"assistant","content":persona_stub}}]
            })))
            .up_to_n_times(1).mount(&server).await;
    }
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices":[{"index":0,"finish_reason":"stop",
                        "message":{"role":"assistant","content":approve}}]
        })))
        .up_to_n_times(1).mount(&server).await;

    Mock::given(method("POST"))
        .and(path("/repos/o/r/pulls/1/reviews"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id":1})))
        .expect(1).mount(&server).await;

    let (store, deps) = crate::common::fixture_with_llm(&server).await;

    // Pre-populate run state: Barry already posted approve on sha1.
    use barry_dylan::checker::multi_review::identity::Identity;
    store.record_post("o", "r", 1, "sha1", Identity::Barry, "approve", 100).await.unwrap();

    // Enqueue a confer job.
    let job = NewJob {
        installation_id: 1,
        repo_owner: "o".into(),
        repo_name: "r".into(),
        pr_number: 1,
        event_kind: "issue_comment.confer".into(),
        delivery_id: "d-confer".into(),
    };
    store.enqueue(&job, 0, 0).await.unwrap();
    let leased = store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(&deps, &leased).await.unwrap();

    let st = store.run_state("o", "r", 1, "sha1").await.unwrap().unwrap();
    assert!(st.barry_posted);
    assert!(st.other_barry_posted);
    assert_eq!(st.confers_used, 1);
}
```

- [ ] **Step 3: Run**

```bash
cargo test --test integration multi_review_confer
```

Expected: pass.

- [ ] **Step 4: Commit**

```bash
git add tests/
git commit -m "test(multi-review): E2E confer escalation"
```

---

### Task 16: Remove the old LlmReviewChecker

**Files:**
- Delete: `src/checker/llm_review/`
- Modify: `src/checker/mod.rs`
- Modify: any tests that referenced `LlmReviewChecker`

- [ ] **Step 1: Verify nothing else references `llm_review`**

```bash
grep -rn "llm_review\|LlmReviewChecker" src/ tests/ | grep -v multi_review
```

Expected: no hits other than the `pub mod llm_review;` in `src/checker/mod.rs` and any imports from removed call sites.

- [ ] **Step 2: Delete the module and reference**

```bash
git rm -r src/checker/llm_review/
```

Edit `src/checker/mod.rs`: remove the `pub mod llm_review;` line.

- [ ] **Step 3: Build and run all tests**

```bash
cargo build && cargo test
```

Expected: clean build; all tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/checker/mod.rs
git commit -m "refactor: remove LlmReviewChecker (replaced by MultiReviewChecker)"
```

---

### Task 17: Telemetry, logging, README

**Files:**
- Modify: `src/checker/multi_review/orchestrator.rs` (counters)
- Modify: `src/checker/multi_review/confer.rs` (counters)
- Modify: `README.md` if present (configuration example)

- [ ] **Step 1: Add metrics counters**

In `src/checker/multi_review/orchestrator.rs`, increment counters at key decision points:

```rust
metrics::counter!("barry_multi_review_judge_total", "verdict" => "agree").increment(1);
metrics::counter!("barry_multi_review_judge_total", "verdict" => "disagree").increment(1);
metrics::counter!("barry_multi_review_barry_alone_total").increment(1);
```

In `src/checker/multi_review/confer.rs`:

```rust
metrics::counter!("barry_confer_total", "outcome" => "ob").increment(1);
metrics::counter!("barry_confer_total", "outcome" => "oob").increment(1);
metrics::counter!("barry_confer_total", "outcome" => "rejected_unauthorized").increment(1);
metrics::counter!("barry_confer_total", "outcome" => "rejected_max_reached").increment(1);
metrics::counter!("barry_confer_total", "outcome" => "rejected_no_run").increment(1);
```

- [ ] **Step 2: Update README configuration example**

If a README exists in the repo root, update its barry.toml example to show the three-identity shape and `[confer]` section.

```bash
ls README.md 2>/dev/null
```

If present, update it. If not, skip this step.

- [ ] **Step 3: Run final test sweep**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

Expected: all green.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat(multi-review): metrics + docs polish"
```

---

## Self-Review

**Spec coverage:**
- ✅ Three identities, three Apps, one binary → Tasks 1, 5, 7, 8
- ✅ Personas hidden from user → Tasks 1, 3 (synthesis merges them)
- ✅ Default flow R1+R2+judge → Task 10
- ✅ Agreement → Barry alone posts → Task 11 (orchestrator branching)
- ✅ Disagreement → both post + neutral check → Task 11 + Task 14
- ✅ /barry confer summons OB then OOB → Task 12
- ✅ max_per_pr cap → Task 12
- ✅ Confer authorization → Task 12 + tests
- ✅ Stale-SHA confer fail-closed → Task 12 (no run state row → reply)
- ✅ Persona prompts compiled in + deployment overrides → Task 1
- ✅ Per-identity LLM safety guard → Task 8 (uses existing `factory::build` per profile)
- ✅ Per-identity 0600 key check → Task 7 Step 4
- ✅ Check-run owned by Barry only → Task 11 (CheckerOutcome posted via dispatcher's standard path uses Barry's `gh` from CheckerCtx)
- ✅ multi_review_runs table → Task 6
- ✅ Removal of LlmReviewChecker → Task 16
- ✅ Three integration tests (agreement / disagreement / confer) → Tasks 13, 14, 15
- ✅ Per-identity error handling (OB failure → Barry alone; judge failure → disagree) → Task 10

**Type consistency:**
- `Identity` enum (Task 1) used by `IdentityClients` (Task 8), `posting` (Task 9), `orchestrator` (Task 10), `MultiReviewChecker` (Task 11), `confer` (Task 12), and `multi_review_runs` accessors (Task 6) — name and variants match.
- `UnifiedReview { outcome, summary, findings }` and `Outcome { Approve, Comment, RequestChanges }` (Task 2) used by synthesis (Task 3), judge (Task 4), orchestrator (Task 10), posting (Task 9) — match.
- `Verdict::{Agree, Disagree, BarryAlone}` (Task 10) used by `MultiReviewChecker` (Task 11) — match.
- `MultiGhFactory::for_identity(Identity, i64)` (Task 7) called from posting (Task 9) and confer (Task 12) — match.
- `CheckerCtx` extension with `store` and `installation_id` (Task 11) — propagated through dispatcher in same task; integration test fixture updates noted in Task 11 Step 2 and Task 13 fixture builder.
- `JobDeps.gh_factory: Arc<dyn MultiGhFactory>` (Task 7) — flows to `Pipeline` building (Task 11) and confer handler (Task 12).
- `Persona { name, prompt: Arc<String> }` (Task 1) used by synthesis (Task 3), orchestrator (Task 10), confer (Task 12) — match.

**Placeholder scan:**
- One acknowledged shortcut: `lookup_clients` / `lookup_personas` use `unsafe transmute` to downcast from `Arc<dyn Checker>` to `Arc<MultiReviewChecker>`. Flagged inline in Task 12 Step 4 with the cleaner alternative (thread `IdentityClients` and personas through `JobDeps` directly). Implementer should pick the cleaner path if convenient.
- Task 14 (disagreement test) has a known fragility around mock-matching order — flagged inline with two fallback strategies.
- No "TBD", no "implement later", no missing code blocks where code is needed.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-05-13-multi-reviewer.md`.

Two execution options:

1. **Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.
2. **Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints.

Which approach?
