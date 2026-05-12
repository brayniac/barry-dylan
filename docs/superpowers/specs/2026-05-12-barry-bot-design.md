# barry-bot — Design

A GitHub bot that watches PRs across one or a few organizations and performs automated review, hygiene checks, and (eventually) testing. v1 ships the foundations end-to-end with LLM-powered review and PR hygiene; later versions add sandboxed runners for lint, tests, and benchmarks.

## Goals

- Install as a GitHub App across one or a few organizations the operator controls.
- Respond to PR open and push events in near-real time via webhooks.
- Post inline LLM code review and PR hygiene findings.
- Be safe to run on PRs from untrusted authors (no auto-trigger for non-maintainers; explicit `/barry approve` opt-in).
- Be small enough to run as a single binary with one SQLite file.
- Provide a clean extension point (the `Checker` trait) so future capabilities (lint, tests, benchmarks) can be added without restructuring the core.

## Non-goals (v1)

- Running PR code (no clippy / cargo test / benchmark execution). Adding this safely requires a sandboxed runner, which is deferred.
- Multi-tenant / Marketplace distribution.
- Horizontal scaling. One process, vertical-scale only.
- Performance regression testing.
- PII / secret scrubbing of diffs before sending to LLMs.

## v1 capability scope

- **LLM review** — inline Pull Request Review comments with overall summary. Posted as `COMMENT` (never `REQUEST_CHANGES`).
- **PR hygiene** — title format, description present / template match, size warning, auto-label by paths changed.

Each capability is implemented as a `Checker`. Results are emitted as Check Runs (one per checker) plus, for `LlmReview`, a PR Review. Labels are applied via the labels API. One-shot notices (trust-gate prompt, command acks) are normal PR comments tagged with hidden HTML markers.

## Architecture

A single Rust binary, `barry-bot`, runs as a long-lived process (systemd unit or container). It holds one SQLite database for operational state (job queue, debounce timers, installation token cache, audit log). Authoritative PR state lives on GitHub; SQLite is purely operational.

```
                       GitHub
                         │
                  HTTPS webhook
                         ▼
   ┌─────────────────────────────────────────────────┐
   │ barry-bot (single process)                      │
   │                                                 │
   │  ┌────────────┐    ┌─────────────────────────┐  │
   │  │  Axum      │    │  Dispatcher (tokio)     │  │
   │  │  webhook   │───►│  - debounce             │  │
   │  │  receiver  │ enq│  - trust gate           │  │
   │  │  (HMAC)    │    │  - fan out to Checkers  │  │
   │  └────────────┘    └────────┬────────────────┘  │
   │                             │                   │
   │                  ┌──────────┴───────────┐       │
   │                  ▼          ▼           ▼       │
   │            Hygiene*    LlmReview    (future)    │
   │             checkers    checker      Clippy …   │
   │                  │          │           │       │
   │                  └─────┬────┴───────────┘       │
   │                        ▼                        │
   │            ┌───────────────────────┐            │
   │            │ GitHub client         │            │
   │            │ (App auth, REST/      │            │
   │            │  GraphQL, retries)    │            │
   │            └───────────────────────┘            │
   │                                                 │
   │  ┌──────────────────┐   ┌──────────────────┐    │
   │  │ SQLite           │   │ LLM client       │    │
   │  │ queue/debounce/  │   │ (Anthropic +     │    │
   │  │ token cache/log  │   │  OpenAI-compat)  │    │
   │  └──────────────────┘   └──────────────────┘    │
   └─────────────────────────────────────────────────┘
```

The webhook handler does only signature verification, enqueue, and return 200. All real work happens in the dispatcher and checkers.

## Module layout

```
src/
├─ main.rs              CLI: `barry-bot run --config barry.toml`
├─ config/              global config + per-repo `.barry.toml` schema
│  ├─ mod.rs
│  └─ repo.rs
├─ webhook/             HTTP intake
│  ├─ server.rs         Axum app, /webhook + /healthz + /metrics
│  ├─ verify.rs         HMAC-SHA256 signature verification
│  └─ event.rs          typed wrapper over received GitHub events
├─ dispatcher/          orchestration
│  ├─ mod.rs            event → job translation, fan-out
│  ├─ debounce.rs       per-PR coalescing
│  ├─ trust.rs          author role gate + `/barry` command parsing
│  └─ worker.rs         tokio worker pool pulling from the SQLite queue
├─ checker/             pluggable capability impls behind a trait
│  ├─ mod.rs            `Checker` trait, `CheckerCtx`, `CheckerOutcome`, `Finding`
│  ├─ hygiene/
│  │  ├─ title.rs
│  │  ├─ description.rs
│  │  ├─ size.rs
│  │  └─ autolabel.rs
│  └─ llm_review/
│     ├─ mod.rs         orchestrates: fetch diff → prompt → parse → outcome
│     ├─ prompt.rs      prompt assembly, diff chunking for large PRs
│     └─ parse.rs       parse model output → inline comments (file, line, body)
├─ github/              all GitHub I/O
│  ├─ app.rs            JWT signing, installation token cache (SQLite)
│  ├─ client.rs         REST + GraphQL wrappers, retry/backoff, rate-limit aware
│  ├─ pr.rs             PR diff, files, prior bot reviews, posting reviews/comments
│  └─ check_run.rs      Checks API helper (status + summary)
├─ llm/                 provider-pluggable inference
│  ├─ mod.rs            `LlmClient` trait, message types
│  ├─ anthropic.rs      /v1/messages
│  └─ openai.rs         /v1/chat/completions (also works for local OpenAI-compat servers)
├─ storage/             SQLite
│  ├─ schema.rs         migrations
│  ├─ queue.rs          enqueue/lease/ack/nack jobs, durable across restarts
│  ├─ tokens.rs         installation token cache
│  └─ audit.rs          who/when/what events
└─ telemetry/           tracing + Prometheus metrics
   └─ mod.rs
```

### Core traits

```rust
#[async_trait]
trait Checker: Send + Sync {
    fn name(&self) -> &'static str;
    fn enabled(&self, repo_cfg: &RepoConfig) -> bool;
    async fn run(&self, ctx: &CheckerCtx) -> Result<CheckerOutcome>;
}

#[async_trait]
trait LlmClient: Send + Sync {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse>;
}
```

`CheckerCtx` carries the GitHub client, the repo's `.barry.toml`, the PR metadata, and the diff. `CheckerOutcome` describes what should be written back (`check_run_status`, `inline_comments`, `labels_to_add`, etc.) — the dispatcher (not the checker) owns all GitHub writes so posting logic stays in one place and checkers are easy to unit-test.

## Data flow

End-to-end lifecycle for a single PR event:

1. **Receive** — GitHub POSTs to `/webhook`. Handler reads the body, verifies `X-Hub-Signature-256` against the webhook secret with constant-time compare. Bad signature → 401, drop.
2. **Classify** — parse event type. v1 handles `pull_request` (opened, synchronize, reopened, ready_for_review) and `issue_comment` (for `/barry` commands on PRs). Everything else → 200 and drop.
3. **Enqueue** — insert a job row `(installation_id, repo, pr_number, event_kind, received_at, run_after)` into SQLite. For `synchronize`, `run_after = now + debounce_secs`. If a pending job already exists for `(repo, pr)`, update its `run_after` to extend the debounce window (coalesce). Return 200.
4. **Lease** — the worker pool polls SQLite for jobs where `run_after <= now AND leased_until IS NULL`. A worker leases the job (`leased_until = now + job_timeout_secs`) and starts processing. Expired leases get retried.
5. **Auth** — exchange the App JWT for an installation token (cached in SQLite until expiry minus 60s). Construct a GitHub client scoped to that installation.
6. **Load context** — fetch in parallel: PR metadata, files changed (with patches), the author's permission level, prior bot reviews/comments, and the repo's `.barry.toml` from the default branch (fall back to bundled defaults if absent).
7. **Trust gate** — if author permission is not write / maintain / admin AND there is no sticky `/barry approve` marker on the PR (a hidden HTML comment in a prior bot comment), post a one-time "needs maintainer approval" comment and stop. Otherwise continue.
8. **Run checkers in parallel** — filter checkers by `enabled(repo_cfg)`, run them concurrently with per-checker timeouts (default 60s for hygiene, 5 min for LLM review). Each returns a `CheckerOutcome` or an error (errors logged; do not fail the whole run).
9. **Aggregate & post** — the dispatcher merges outcomes:
   - **One Check Run per checker** (Checks API): `success` / `neutral` / `failure` with a short summary.
   - **One PR Review** for `LlmReview`: inline comments anchored to `(file, position)` pairs plus an overall body, posted with event `COMMENT`.
   - **Labels** from `autolabel` outcome.
   - **PR comments** only for things that don't fit a Check Run (trust-gate notice, command acks).
10. **Mark complete** — delete the job row on success, or increment `attempts` and reschedule with capped exponential backoff on failure (max 3 attempts).
11. **Audit** — append a row to `audit_log` summarizing what ran, durations, and outcomes.

### Comment / review idempotency

Re-runs are expected on every push.

- **LLM reviews** — include a hidden marker `<!-- barry-bot:llm-review:v1 -->` in the review body. Before posting a new review, the previous one (matched by marker) gets minimized via the GraphQL `minimizeComment` mutation (UI shows it as "outdated"). The new review posts fresh.
- **Hygiene findings** — emitted as Check Runs, which GitHub replaces in place when re-run with the same `name`.
- **Labels** — idempotent by API contract.
- **One-shot notices** — tagged with hidden markers and never re-posted.

### Big-PR handling for LLM review

If the diff exceeds the model's effective context budget (configurable per-repo, default 100k tokens), chunk by file:

- Review files in groups whose patches fit one call. Each call yields inline comments directly.
- Then run one final "synthesis" call over the per-file outcomes to produce the overall review body.
- Files larger than a single-call budget get a single "this file is too large to review automatically" inline comment at the top of that file.

### `/barry` commands

Issue-comment events whose body starts with `/barry` are parsed in `dispatcher/trust.rs`:

- `/barry approve` (maintainer only) — write a hidden marker into a fresh bot comment, then enqueue a normal review job. The marker makes the PR trusted for all subsequent pushes.
- `/barry review` — enqueue a review job (still trust-gated; only effective for trusted authors).
- Anything else — react `👎` on the comment so the author knows it parsed but is unknown.

Author permission level is checked against the GitHub API each time, not cached.

## Error handling

| Source | Policy |
|---|---|
| Bad webhook signature | 401, drop, warn log |
| Unsupported / uninteresting event | 200, drop, debug log |
| SQLite enqueue failure | 500 to GitHub (it retries delivery on 5xx); alert if sustained |
| Installation token mint failure (GitHub 5xx / clock skew) | Job nack, exponential backoff (1m / 5m / 25m), drop after 3 attempts, audit log entry |
| GitHub API 4xx (permissions, gone, etc.) | Don't retry; record in audit log; mark job complete |
| GitHub API 5xx / 429 (secondary rate limit) | Respect `Retry-After` if present, else exponential backoff. Consume from `X-RateLimit-Remaining`; pause workers near zero |
| Repo missing `.barry.toml` | Use bundled defaults; do not error |
| Malformed `.barry.toml` | Post one Check Run named `barry/config` with `failure` and the parse error; skip all other checkers until fixed (idempotent — re-runs replace the same Check Run) |
| Individual checker panic / error | Catch at the checker boundary in the dispatcher, mark that checker's Check Run as `neutral` with "internal error" summary, continue with other checkers |
| LLM provider 5xx / timeout | Retry within the LLM call (3 attempts, jittered backoff); on final failure, post `barry/llm-review` Check Run as `neutral` with the error message |
| LLM produces unparseable output | Same as above — `neutral` Check Run, don't post a malformed review |
| Lease expired (worker crashed mid-job) | Another worker picks it up on next poll; `attempts` increments — same backoff as nack |

Failure-isolation principle: a bug in any single checker, repo, or PR must never wedge the bot or block other PRs. The dispatcher catches and isolates failures at the checker boundary.

## Security

- **Webhook auth** — HMAC-SHA256 with constant-time compare. Secret rotates via config reload (SIGHUP).
- **App private key** — loaded from a file path in config; never logged. Path permissions are checked on startup (must be 0600 or stricter); refuse to start otherwise.
- **Installation tokens** — short-lived (~1h), cached in SQLite, never logged.
- **App permissions requested** —
  - Pull requests: R/W (post reviews, labels)
  - Contents: R (read `.barry.toml`, diffs)
  - Issues: R/W (commands via issue_comment, labels)
  - Checks: R/W
  - Metadata: R
  - Webhook subscriptions: `pull_request`, `issue_comment`. Nothing else — explicitly no `Actions`, `Administration`, or `Secrets`.
- **Untrusted PR content** — v1 does no code execution. The only attack surface is "what gets included in an LLM prompt." Diff content is treated as untrusted data and never allowed to influence control flow. Prompts are constructed with clear delimiters around untrusted content.
- **`/barry` commands** — author permission level is checked against GitHub each time, not cached. Trust signals are not carried across pushes except via the explicit sticky `/barry approve` marker.
- **LLM endpoint guard** — if the configured endpoint is non-Anthropic but the configured provider type is `anthropic`, refuse to start. Fail-loud rather than leak prompts to an unexpected service.
- **PII / secrets in diffs** — diffs are sent to whichever LLM endpoint is configured. Document this in the README. No client-side scrubbing in v1.

## Configuration

Global config `barry.toml` (server-side; secrets via env vars):

```toml
[server]
listen = "0.0.0.0:8080"
public_url = "https://barry.example.com"   # informational

[github]
app_id = 123456
private_key_path = "/etc/barry/app.pem"
webhook_secret_env = "BARRY_WEBHOOK_SECRET"

[storage]
sqlite_path = "/var/lib/barry/barry.db"

[llm.default]
provider = "anthropic"                     # or "openai"
endpoint = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"          # optional — omit for unauthed local endpoints
model = "claude-sonnet-4-6"
max_tokens = 8192
request_timeout_secs = 300

[dispatcher]
debounce_secs = 30
worker_count = 4
job_timeout_secs = 600

[defaults]                                 # used when a repo has no .barry.toml
# mirrors the per-repo schema below
```

Per-repo `.barry.toml`:

```toml
[hygiene.title]
enabled = true
pattern = "^(feat|fix|chore|docs|refactor|test|perf)(\\([^)]+\\))?: .+"

[hygiene.description]
enabled = true
min_length = 20
require_template_sections = []             # e.g. ["## Summary", "## Test plan"]

[hygiene.size]
enabled = true
warn_lines = 500
warn_files = 20

[hygiene.autolabel]
enabled = true
rules = [
  { paths = ["src/agent/samplers/bpf/**"], labels = ["area/bpf"] },
  { paths = ["src/viewer/**"],             labels = ["area/viewer"] },
]

[llm_review]
enabled = true
# provider_profile = "default"             # references [llm.<name>] from global
focus = """
You are reviewing a Rust systems performance project. Prioritize:
- correctness of unsafe / FFI code
- lock-free / concurrency patterns
- eBPF program safety
"""
exclude_paths = ["docs/**", "**/*.md"]
max_diff_tokens = 100000
```

## Operations

- **Logs** — structured JSON via `tracing`. One line per webhook + one per job lifecycle event (received / leased / completed / failed). Correlated by a `request_id` derived from the GitHub `X-GitHub-Delivery` header and propagated through every downstream call.
- **Metrics** — Prometheus endpoint on `/metrics`: webhooks received (by event type), jobs enqueued / completed / failed, job durations, LLM tokens in/out, GitHub API requests by status code, queue depth, GitHub rate-limit remaining.
- **Healthcheck** — `/healthz` returns 200 if SQLite is writable, an App JWT can be minted, and the configured LLM endpoint is reachable. Result is cached for 30s.
- **Reload** — SIGHUP re-reads `barry.toml`. Per-repo config is fetched live on every job, so reload doesn't affect repo policy.

## Testing strategy

### Unit tests

- `webhook/verify.rs` — HMAC verification against known-good and tampered payloads (fixtures: real GitHub deliveries with their signatures).
- `dispatcher/trust.rs` — author role + sticky `/barry approve` marker → trusted/untrusted decision table.
- `dispatcher/debounce.rs` — coalescing logic via a fake clock.
- `checker/hygiene/*` — input PR fixture + repo config → expected `CheckerOutcome`, no GitHub I/O.
- `checker/llm_review/prompt.rs` — diff chunking: diffs of various sizes and token budgets → expected chunking.
- `checker/llm_review/parse.rs` — fixtures for good, malformed, and edge-case model outputs → expected comments or a clean error (never panics).
- `config/*` — round-trip parsing and validation: good TOML parses, bad TOML produces a useful error.
- `github/app.rs` — JWT signing + installation-token caching with a fake clock.
- `storage/queue.rs` — enqueue / lease / debounce-coalesce / lease-expiry recovery against an in-memory SQLite.

### Integration tests

A `wiremock`-based GitHub mock serves canned responses for `/repos/.../pulls`, `/installations/<id>/access_tokens`, `/repos/.../check-runs`, etc. Tests stand up the real Axum server, POST recorded webhook payloads, and assert the mock observed the expected outgoing calls.

A similar wiremock for LLM endpoints (Anthropic and OpenAI shapes) with canned responses — one test per provider.

Required integration scenarios:

- Happy path: `pull_request.opened` → hygiene Check Runs created + labels applied + LLM review posted.
- Trust gate: untrusted author → only the "needs approval" comment is posted.
- Re-run idempotency: deliver `synchronize` twice in quick succession → exactly one job runs, prior LLM review is minimized exactly once.
- Failure isolation: one checker errors → other checkers still post.

### End-to-end smoke test (manual, documented)

The README documents how to register a dev GitHub App, point it at a localhost tunnel (smee.io or ngrok), open a PR on a sandbox repo, and verify each output type appears. This is the final pre-deploy gate; not automated.

### Not tested in v1

Real GitHub or real LLM calls in CI. Flaky, gated on secrets, and don't catch anything the wiremock tests miss for the logic in scope.

### Test fixtures

`tests/fixtures/` holds:

- recorded GitHub webhook payloads (one per event type/action handled),
- matching API responses the mock should serve,
- canned LLM responses (good, malformed, oversized).

Fixtures are recorded once against a real sandbox account during initial development and committed.

## Open questions

None at design time.

## Future work (post-v1)

- Sandboxed runner host (ephemeral VMs / Firecracker / nsjail) to safely execute PR code.
- Lint checker (`cargo clippy`, generalizable to other ecosystems).
- Test checker (`cargo test`, generalizable to other ecosystems).
- Benchmark checker with baseline-vs-PR comparison and a stable runner host.
- Provider-pluggable LLM trust controls (e.g. per-repo allowlist of LLM endpoints).
- Backfill / replay command for re-running a single webhook delivery from the audit log.
