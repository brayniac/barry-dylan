# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Barry Dylan is a GitHub App that runs automated PR review across one or a few organizations. It uses a single Rust binary with embedded SQLite and is webhook-driven.

The "Barry & Other Barry" multi-reviewer feature gives each PR two independent LLM reviewers (different model providers, different personas — security, correctness, style — merged internally), a peer round in which each reads the other's review and revises, and a hidden judge that decides whether the revised reviews materially agree. Disjoint findings are complementary, not a disagreement; only a different outcome or a contradiction is. When they agree, only Barry posts. When they disagree, both post and the check-run goes neutral. A maintainer can summon a third reviewer ("Other Other Barry") with `/barry confer`.

## Build and Run Commands

```bash
# Build the project (debug mode)
cargo build

# Build for release
cargo build --release

# Run tests
cargo test

# Run specific test
cargo test test_name

# Run integration tests
cargo test --test integration
```

### Running the App

1. **Register three GitHub Apps**, one per reviewer identity (Barry, Other Barry, Other Other Barry). Each needs PR read/write, contents read, checks write, and issues/comments write. Only **Barry** subscribes to webhook events (`pull_request` and `issue_comment`).

2. **Install all three Apps** on the repos you want reviewed.

3. **Generate config files** — copy `config/barry.toml.example` to `barry.toml` and fill in the three App IDs, three private key paths, and SQLite path.

4. **Set env vars**:
   ```bash
   export BARRY_WEBHOOK_SECRET=<your webhook secret>
   export ANTHROPIC_API_KEY=<your key>      # if used by any [llm.*] profile
   export OPENAI_API_KEY=<your key>         # if used by any [llm.*] profile
   ```

5. **Start**:
   ```bash
   cargo run --release -- run --config barry.toml
   ```

6. **Expose the webhook endpoint** (Barry's App only). For dev, use [smee.io](https://smee.io) or [ngrok](https://ngrok.com) to forward a public URL to `http://localhost:8181/webhook`.

## Architecture

### Core Components

```
src/
├── app_runtime.rs    # App startup, config loading, worker pool, HTTP server
├── checker/          # PR checkers (hygiene, multi-review LLM)
├── config/           # Configuration parsing and validation
├── dispatcher/       # Job queue, leasing, worker execution
├── github/           # GitHub API clients, GraphQL/REST wrappers
├── llm/              # LLM client abstractions (Anthropic, OpenAI)
├── storage/          # SQLite actor with blocking thread
├── telemetry/        # Tracing and metrics setup
└── webhook/          # Webhook event handling, verification
```

### Key Architectural Patterns

1. **Single-actor SQLite storage** (`src/storage/actor.rs`): All database access goes through a single blocking thread via message passing. The `Store` struct holds a sender to the actor and a read cache.

2. **Job queue pattern** (`src/storage/queue.rs`): Jobs are leased with a timeout, allowing multiple workers to process events concurrently without duplicates.

3. **Multi-GitHub factory** (`src/app_runtime.rs:AppGhFactory`): Implements `MultiGhFactory` trait to mint GitHub clients for different identities (Barry, Other Barry, Other Other Barry) per installation.

4. **Pipeline checkers** (`src/checker/`): Each checker implements a trait with `name()`, `enabled()`, and `run()`. The `MultiReviewChecker` runs parallel LLM reviews from different providers and a hidden judge.

5. **LLM client abstraction** (`src/llm/`): Unified interface for Anthropic and OpenAI, with retry logic for transient errors.

6. **Trust gate** (`src/dispatcher/trust.rs`): Untrusted PRs (from non-maintainers) require `/barry approve` from a maintainer before LLM review runs.

7. **No stale reviews** (`src/dispatcher/cancel.rs`, `src/dispatcher/run.rs`): a `synchronize` webhook fires the PR's cancel token immediately, dropping the in-flight review of the old head (and cancelling its rack experiment); a `closed` webhook does the same via the queue. `post_outcome` re-fetches the PR before writing anything and skips the post if the head moved or the PR is no longer open.

8. **Reviews survive a restart** (`src/rack/mod.rs::review_for_job`, `src/dispatcher/worker.rs`): the rack experiment id is persisted on the `jobs` row (`rack_experiment`) on submit and cleared when it ends. On SIGTERM the worker drops the job in hand and releases its lease (`reschedule_at`, no attempt spent); the abandon guard sees the shutdown token and does not cancel the experiment; the next process leases the job and resumes the experiment instead of resubmitting. A resumed experiment that ended badly is resubmitted once.

### Slash Commands

Every comment command is gated at the webhook on `commands_from` (GitHub logins, checked against the signed comment author). Absent means nobody can command Barry from a comment.

- `/barry approve` — maintainer-only; trusts the PR author for this PR so reviews start running. Sticky for the lifetime of the PR.
- `/barry review` — re-runs the full review pipeline on the current head.
- `/barry confer` — summons the next un-posted Barry (OB then OOB) to post an independent review on the current head SHA.
- `@barry-dylan` — same as `/barry review`. A commander's comment also gets through the `repos` allowlist, so this is how to get one review of a PR in a repository Barry does not otherwise act on.

### Configuration

- `barry.toml`: Global config with server listen address, GitHub App credentials (3 apps), storage SQLite path, LLM profiles (barry, other_barry, other_other_barry, judge), dispatcher settings (workers, timeouts), and confer rules.
- `.barry.toml`: Per-repo config for disabling checkers or overriding defaults.

### Metrics

`/metrics` (Prometheus) exposes:
- `barry_multi_review_judge_total{verdict="agree"|"disagree"}`
- `barry_multi_review_barry_alone_total` — Other Barry was unreachable
- `barry_multi_review_peer_round_total{outcome="revised"|"kept_first"}` — the peer round produced a revised review, or that reviewer kept its first
- `barry_confer_total{outcome="ob"|"oob"|"rejected_unauthorized"|"rejected_max_reached"|"rejected_no_run"|"rejected_all_posted"}`
- `barry_webhook_command_total{outcome="accepted"|"on_demand"|"rejected"}` — comment commands, `on_demand` being one in a repository outside `repos`
- `barry_review_superseded_total{by="push"}` — a push cancelled the review in flight for the old head
- `barry_review_stale_total{reason="head_moved"|"closed"}` — outcome not posted because the PR moved on
- plus the usual job/webhook counters.

### Security

- Diffs are sent to configured LLM endpoints. Do not run on repos containing secrets/PII.
- Each App private key file must have mode `0600` or stricter.
- Webhooks are HMAC-SHA256 verified with constant-time compare.
- Provider/endpoint validation prevents misconfiguration leaks.
