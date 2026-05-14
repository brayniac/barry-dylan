# Multi-Reviewer ("Barry and Other Barry") Design

**Status:** Proposed
**Date:** 2026-05-13
**Author:** Brian Martin (with Claude)

## Summary

Replace the single `LlmReviewChecker` with a multi-identity, multi-persona reviewer that surfaces a second voice only when two independent LLM reviews materially disagree. Three GitHub App identities — Barry, Other Barry, OOB — each with their own LLM config, post under their own avatars when summoned. Personas (security, correctness, style) are an internal prompting concern and are not visible to PR readers.

## Goals

- Higher review signal by cross-checking with a second model on every PR.
- Low noise: most PRs see one voice (Barry); the second voice (Other Barry) appears only when there's real disagreement.
- Visible disagreement when it happens, with explicit "I disagree because…" framing under a distinct identity.
- User-controllable escalation via `/barry confer` to summon another Barry into the thread.
- Each Barry is a real, separate posting identity in the GitHub PR UI (distinct App, distinct avatar).
- Personas are hidden — readers see one unified review per Barry, not per-facet checklists.

## Non-goals

- Per-repo configuration of personas, prompts, or confer policy. All policy is operator-set.
- User-extensible personas. Adding a persona is a code change.
- Flag-gated rollout. The multi-review pipeline replaces the single-review one wholesale.
- Cross-installation fleet coordination. This is a personal dogfood bot.
- Real-LLM correctness assertions in tests. Prompt quality is evaluated through dogfooding.

## Architecture

### Three identities, three Apps, one binary

Three GitHub Apps (Barry, Other Barry, OOB) backed by one bot binary. Each App has its own `app_id`, private key, and avatar. The binary holds three sets of credentials and mints installation tokens per identity on demand.

Webhooks arrive only on Barry's App (configured as the canonical webhook receiver). OB and OOB Apps exist solely to mint installation tokens for posting and to own their visual identity in the PR UI.

Each identity has its own `[llm.*]` config — host, model, optional API key. They are independently configurable so a deployment can pair a frontier-model Barry with a local Other Barry; the disagreement is itself the signal that two distinct minds saw the same code.

### Personas are hidden

Each Barry, when called on, internally runs N persona prompts (security, correctness, style) over the diff, then synthesizes them into one unified review that posts under that Barry's identity. The user sees one review per Barry, never the per-persona drafts.

The persona set is fixed in code. Default persona prompts are compiled in via `include_str!`, with optional deployment-level prompt overrides per persona (see Configuration). All personas always run; there is no enable/disable knob.

### Default flow per PR event

1. **R1 (parallel).** Barry and OB each produce a unified review internally (per-persona LLM calls → synthesis call). Nothing posted yet.
2. **R2 (parallel).** Each reads the other's R1 and may revise. Still nothing posted.
3. **Judge pass.** A judge prompt reads both R2s and decides whether they materially agree (same outcome, overlapping concerns).
   - **Agree** → Barry posts their R2. OB stays silent. `barry/llm-review` check-run reflects Barry's outcome.
   - **Disagree** → Barry posts their R2 *and* OB posts their R2 with explicit "I disagree with Barry on X because Y" framing. `barry/llm-review` check-run is set to **neutral** ("no consensus, human review needed").

### `/barry confer` (escalation flow)

A PR comment matching `^/barry confer\s*$` from an authorized user summons another Barry:

- If OB has not posted yet → run OB now. OB posts agreeing-or-disagreeing with Barry. (The user explicitly asked for the second voice, so OB posts even when they agree.)
- If OB has already posted → run OOB. OOB reads Barry + OB and posts its own take.
- If OOB has already posted → reply "all Barrys already conferred", no-op.

After confer, `barry/llm-review` is updated to reflect the majority outcome of all posted Barrys.

### Why this shape works

- Most PRs: user sees one voice (Barry). Quiet, clean.
- Real disagreements: both voices visible, check-run honest about the lack of consensus.
- The persuasion+judge gate filters out borderline phrasing differences — only substantive splits surface a second voice.
- Users can pull in more voices on demand. The "cast" is summoned, not always on stage.

### Module layout

```
src/checker/multi_review/
  mod.rs           — MultiReviewChecker; impls Checker trait (replaces LlmReviewChecker slot)
  orchestrator.rs  — runs R1/R2/judge, decides what posts
  personas.rs      — built-in persona prompts; resolves operator overrides at startup
  synthesis.rs     — per-Barry merge of persona drafts → one unified review
  judge.rs         — agree-or-disagree pass over two R2 reviews
  confer.rs        — handles /barry confer command (escalates OB or OOB)
  posting.rs       — posts each Barry's review under its own App identity
  prompts/
    security.md
    correctness.md
    style.md
```

### Multi-App plumbing

Extend `GhFactory` to `MultiGhFactory` with:

```rust
trait MultiGhFactory: Send + Sync {
    // Existing single-identity method (used by setup phase, check-run owner)
    async fn for_installation(&self, installation_id: i64) -> Result<Arc<GitHub>>;

    // New: identity-scoped client
    async fn for_identity(&self, identity: Identity, installation_id: i64) -> Result<Arc<GitHub>>;
}

enum Identity {
    Barry,
    OtherBarry,
    OtherOtherBarry,
}
```

The orchestrator holds an `Arc<GitHub>` per Identity for the current installation.

### Check-run ownership

`barry/llm-review` is owned exclusively by Barry's App. GitHub check-runs are owned by one App; Barry's identity sets and updates the check regardless of which Barrys posted reviews.

## Configuration

All configuration lives in `barry.toml` (operator-managed, gitignored). This feature touches no per-repo `.barry.toml` config.

```toml
[github.barry]
app_id = 123456
private_key_path = "/etc/barry/barry.pem"

[github.other_barry]
app_id = 234567
private_key_path = "/etc/barry/other-barry.pem"

[github.other_other_barry]
app_id = 345678
private_key_path = "/etc/barry/oob.pem"

[llm.barry]
host = "https://api.anthropic.com"
model = "claude-opus-4-7"
api_key_env = "BARRY_ANTHROPIC_KEY"

[llm.other_barry]
host = "http://localhost:11434"
model = "qwen2.5-coder:32b"
# no api_key for local

[llm.other_other_barry]
host = "https://api.openai.com"
model = "gpt-5"
api_key_env = "OOB_OPENAI_KEY"

[judge]
host = "https://api.anthropic.com"
model = "claude-haiku-4-5-20251001"
api_key_env = "BARRY_ANTHROPIC_KEY"

[personas.security]
# prompt_path = "/etc/barry/prompts/security.md"  # optional override

[personas.correctness]

[personas.style]

[confer]
allowed = ["author", "write", "admin"]
max_per_pr = 2  # one for OB, one for OOB
```

### Persona prompts

Built-in personas with default prompts compiled in. Operators may override the prompt text per persona via `prompt_path`. The set of personas (which exist, what they're named) is fixed in code:

```rust
pub struct Persona {
    pub name: &'static str,
    pub default_prompt: &'static str,
}

const BUILT_IN_PERSONAS: &[Persona] = &[
    Persona { name: "security",    default_prompt: include_str!("prompts/security.md") },
    Persona { name: "correctness", default_prompt: include_str!("prompts/correctness.md") },
    Persona { name: "style",       default_prompt: include_str!("prompts/style.md") },
];
```

At startup each persona's effective prompt = operator override if present, else `default_prompt`. Resolved once into `Arc<[PersonaConfig]>` and passed to the orchestrator.

### Confer policy

Uniform across all installations. `allowed` lists the GitHub permission roles that can summon a Barry; `max_per_pr` caps the total confers per PR head SHA (default 2: one for OB, one for OOB). Drive-by external contributors cannot confer.

### Startup validation

Bot refuses to start unless all of the following pass:

- All three pem files have mode 0600 or stricter (existing `ensure_key_mode_strict` extended to all three).
- Each `[llm.*]` block passes the existing factory safety guard (e.g., `provider=anthropic` requires `host == api.anthropic.com`).
- Each `[personas.*].prompt_path` (if set) is readable.
- `[confer].allowed` values are valid GitHub permission roles.

## Request flow

Two trigger paths into the same orchestrator: PR events (default flow) and `/barry confer` comments (escalation flow).

### Default flow (PR opened/synchronized)

```
GitHub webhook → /webhooks endpoint
  → verify HMAC (constant-time) [existing]
  → enqueue Job{repo, pr, sha, installation_id} [existing]

Worker picks up Job
  → JobDeps{store, config, pipeline, gh_factory}
  → Setup phase [existing]:
      ├─ MultiGhFactory mints Barry's install token
      ├─ tokio::join!(fetch_pr_context, list_pr_files) via Barry's GH client
      └─ author_permission lookup
  → Pipeline runs MultiReviewChecker

MultiReviewChecker.check(ctx)
  → orchestrator.run_default(ctx)
      ├─ Mint OB install token via MultiGhFactory
      ├─ R1 (parallel):
      │     tokio::join!(
      │       barry_r1 = run_persona_synthesis(barry_llm, ctx),
      │       ob_r1    = run_persona_synthesis(ob_llm, ctx),
      │     )
      ├─ R2 (parallel):
      │     tokio::join!(
      │       barry_r2 = revise(barry_llm, barry_r1, ob_r1),
      │       ob_r2    = revise(ob_llm,    ob_r1,    barry_r1),
      │     )
      ├─ Judge: judge.materially_agree(barry_r2, ob_r2) → bool
      └─ Post:
           agree    → posting.post_as(Barry, barry_r2)
                      conclusion = barry_r2.outcome
           disagree → posting.post_as(Barry, barry_r2)
                      posting.post_as(OB, ob_r2_with_disagreement_framing)
                      conclusion = Neutral
  → Update barry/llm-review check-run via Barry's GH client
  → Persist run state to store: identities posted, conclusion, head_sha
```

### Confer flow (`/barry confer` comment)

```
GitHub webhook (IssueCommentEvent) → /webhooks
  → verify HMAC [existing]
  → if comment matches /^\/barry confer\s*$/ → enqueue ConferJob

Worker picks up ConferJob
  → Setup phase [existing] mints Barry's install token
  → confer.handle(ctx):
      ├─ Authorize: comment_author role in confer.allowed?
      │     no  → posting.react(👎) on the comment, done
      │     yes → continue
      ├─ Load run state from store (Barry's last review, identities posted)
      │     if no prior run for this PR head SHA → reply "no review to confer on, run again first", done
      ├─ confers_used = store.confers_for(pr, sha)
      │     if >= max_per_pr → reply "max confers reached", done
      ├─ Decide identity:
      │     if OB hasn't posted → run OB, post as OB
      │     elif OOB hasn't posted → run OOB, post as OOB
      │     else → reply "all Barrys already conferred", done
      ├─ Run review:
      │     read Barry's posted review (+ OB's if OOB's turn)
      │     run_persona_synthesis(identity_llm, ctx, prior_reviews=[…])
      │     post under that identity with explicit agree/disagree framing
      └─ Update barry/llm-review conclusion = majority_of_posted_barrys
      → Persist updated run state
```

### State boundaries

| State | Where it lives |
|---|---|
| Bot config (Apps, LLMs, judge, confer policy) | `Arc<BotConfig>`, loaded once at startup |
| Per-installation GH clients (3 identities) | `MultiGhFactory`, cached by `(identity, installation_id)`, refresh on token expiry |
| Per-PR-head run state (which Barrys posted, confer count) | sqlite `multi_review_runs` table, keyed by `(repo_id, pr, head_sha)` |
| Persona prompts | Resolved at startup into `Arc<[PersonaConfig]>` |

Run state is keyed on head SHA. A new push → new head SHA → no row exists → confer on that SHA fails closed with "no review to confer on" until the default flow has run for the new SHA. Confer always runs fresh (it doesn't replay cached reasoning) — the framing of a confer review is different from a default-flow review (explicit agree/disagree to a posted peer), so reusing the in-memory R2 wouldn't be correct anyway.

### Concurrency

- **Within a single PR event:** `tokio::join!` for parallel R1, parallel R2. Personas within a single Barry also run via `tokio::join!`. So one event fans out to ~6 concurrent LLM calls (3 personas × 2 identities) for R1 alone.
- **Across PR events:** one job per worker; existing serialization per-PR prevents R1 of event N+1 from racing R2 of event N.
- **Per-identity rate limiting:** each App has its own 5000/hr GitHub budget. The factory tracks usage per identity.

### Error handling

| Failure | Behavior |
|---|---|
| OB's LLM call fails (R1 or R2) | Treat as "OB unavailable" — Barry posts alone, check-run = Barry's outcome, log warning. Do not fail the job. |
| Barry's LLM call fails | Job fails, retries via existing retry logic. (Barry is canonical; without Barry we don't post.) |
| Judge call fails | Default to "disagree" (post both reviews). Conservative — surfaces ambiguity rather than hides it. |
| OB or OOB token mint fails | Skip that identity, fall through to "Barry alone" posting. Log loudly. |
| Confer comment from unauthorized user | Single 👎 reaction, no further action. No reply (avoids comment-spam loops). |
| New SHA pushed between default flow and confer | No run-state row exists for the new SHA. Reply "PR was updated since the last review, run again first", no-op. |

## Testing

Orchestrator and judge are pure logic over `LlmClient` + `GhClient` traits — both already have mock impls in the existing pipeline. Most coverage is unit-level.

### Unit tests (no network)

| What | Where | What it pins |
|---|---|---|
| Persona+synthesis pipeline produces one unified review per Barry | `personas.rs` / `synthesis.rs` | Persona drafts merge correctly; outcome derived from worst-case persona finding |
| Judge classifies "same outcome + overlapping concerns" as agree | `judge.rs` | Borderline phrasing differences don't escalate |
| Judge classifies "different outcome" as disagree | `judge.rs` | Real splits do escalate |
| Judge classifies "same outcome, disjoint concerns" as disagree | `judge.rs` | Two Barrys finding *different* issues is a substantive disagreement worth surfacing |
| Orchestrator default flow: agree → only Barry posts | `orchestrator.rs` | Agreement path quiet |
| Orchestrator default flow: disagree → both post, check = Neutral | `orchestrator.rs` | Disagreement path correct |
| Orchestrator: OB LLM failure → Barry alone, no job failure | `orchestrator.rs` | Degraded mode |
| Orchestrator: judge failure → defaults to disagree | `orchestrator.rs` | Conservative fallback |
| Confer authz: write/admin/author allowed | `confer.rs` | Trust gate matches policy |
| Confer authz: drive-by contributor rejected with 👎 reaction | `confer.rs` | Trust gate rejects |
| Confer state machine: nothing posted → run OB | `confer.rs` | First-confer path |
| Confer state machine: OB posted → run OOB | `confer.rs` | Second-confer path |
| Confer state machine: OOB posted → no-op reply | `confer.rs` | Cap respected |
| Confer state machine: max_per_pr respected | `confer.rs` | Hard cap |
| Confer with no run-state row for current head SHA → "run again first" reply, no run | `confer.rs` | Stale-review handling |
| MultiGhFactory mints distinct tokens per identity | `multi_gh_factory.rs` | Identity isolation |
| Startup validation rejects 0644 pem | (existing) | Reuse existing key-mode test for each pem |
| Startup validation rejects `provider=anthropic` + non-anthropic host per identity | (existing) | Factory safety guard runs per `[llm.*]` |

### Integration tests (sqlite + mock LLM + mock GitHub)

Three end-to-end scenarios that exercise the full webhook → worker → orchestrator → store path:

1. **PR opened, agreement** — webhook in, R1+R2+judge run with mock LLMs returning identical outcomes; exactly one PR review posted under Barry's identity; check-run = success.
2. **PR opened, disagreement** — R2 outcomes differ; two reviews posted (Barry + OB); check-run = neutral; run state persisted with both Barrys recorded.
3. **Confer escalation** — agreement scenario followed by `/barry confer` comment from authorized user → OB review posted; second `/barry confer` → OOB review posted; third `/barry confer` → "all conferred" reply.

### What we explicitly don't test

- Real LLM calls. Persona prompt quality is evaluated through dogfooding (Barry reviewing Barry's PRs), not assertions. Tests pin orchestration shape, not LLM judgment.
- Real GitHub API. Existing mock harness covers this layer.
- Cross-installation behavior (single repo install is fine for v1).

## Rollout & migration

This replaces an existing single-checker (`LlmReviewChecker`) with `MultiReviewChecker` in the same pipeline slot. Migration is mostly operator setup, not code shimming.

### Code-side changes

- New `src/checker/multi_review/` module (per the layout above).
- `LlmReviewChecker` removed entirely — no compatibility shim. The trait slot it occupied is taken by `MultiReviewChecker`.
- New sqlx migration: `multi_review_runs` table for per-PR run state.
- `JobDeps` switches from `gh_factory: Arc<dyn GhFactory>` to `gh_factory: Arc<dyn MultiGhFactory>`. The single-identity method stays for the setup phase and check-run ownership; a new `for_identity` method is added.
- `barry.toml` parsing: new sections become required. Bot refuses to start without them.

### Operator-side rollout (one-shot, sequential)

1. Register two new GitHub Apps in the org (Other Barry, OOB) with the same permissions and webhook URL as Barry.
2. Generate avatars for OB and OOB (entertainment requirement — visually distinct).
3. Install both new Apps on every repo where Barry is installed.
4. Add three pem files to the bot host (chmod 0600 each).
5. Update `barry.toml` with the new `[github.*]`, `[llm.*]`, `[judge]`, `[confer]`, `[personas.*]` sections.
6. Deploy new binary. Bot validates config at startup.
7. First PR event after deploy exercises the new flow.

### No flag-gated rollout

This is a behavior change, not a risk-tunable one. Either the bot runs the multi-review pipeline or it doesn't. Operators who want to roll back deploy the previous binary.

### Backward compatibility

None required. This is a personal/dogfood bot; the user is the operator. There is no fleet to coordinate.

### Cost change at rollout

Per PR event, LLM calls go from 1 → 11:
- R1: 3 personas + 1 synthesis per Barry × 2 Barrys = 8 calls
- R2: 1 revise call per Barry = 2 calls
- Judge: 1 call

Add 4 more per `/barry confer` (3 personas + 1 synthesis for the summoned Barry). With both confers used (max_per_pr = 2), worst case is 19 calls per PR.

Operator-tunable via the `[llm.*]` `model` choice — pointing Other Barry at a local model keeps marginal cost near zero on the dominant path.

## Open questions

None at design time. Specific implementation choices (e.g., exact judge prompt, exact persona prompt wording) are deferred to the implementation plan and will be iterated through dogfooding.
