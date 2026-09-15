# Barry Dylan

Automated PR review powered by multiple LLM reviewers — single Rust binary, embedded SQLite, webhook-driven.

Barry Dylan deploys three GitHub App identities (Barry, Other Barry, Other Other Barry), each with its own LLM provider and review persona. A hidden judge decides whether the two primary reviewers materially agree. When they do, only Barry posts. When they disagree, both post independently and the check-run goes neutral.

## Status

**v1 ships:**
- Multi-reviewer LLM review (two visible identities + hidden judge + optional confer)
- PR hygiene: title format, description, size warning, auto-labels
- Trust gate: untrusted PRs require `/barry approve` from a maintainer
- `/barry review`, `/barry approve`, `/barry confer` slash commands
- Cancellation on PR close/merge, job coalescing, retry with backoff
- Prometheus metrics + structured JSON logs

## Architecture

```
src/
├── app_runtime.rs    # App startup, config, worker pool, HTTP server
├── checker/          # PR checkers (hygiene + multi-review LLM)
├── config/           # Configuration parsing and validation
├── dispatcher/       # Job queue, leasing, worker execution
├── github/           # GitHub API clients, GraphQL/REST wrappers
├── llm/              # LLM client abstractions (Anthropic, OpenAI)
├── storage/          # SQLite actor with blocking thread
├── telemetry/        # Tracing and metrics setup
└── webhook/          # Webhook event handling, verification
```

### Where the Models Run

By default each identity calls its `[llm.*]` endpoint directly. With a `[rack]` section the reviews instead run as a systemslab experiment: an ephemeral GPU VM pulls the model, serves it, produces both reviews, and is destroyed. Barry never speaks to the model.

`[rack] judge = true` moves the judge in there too, against the model already loaded. That removes the last remote call: **no `[llm.*]` profile is required at all**, no API key lives in `secrets.env` but the webhook secret, and no diff leaves the rack. It needs two reviewers and the default sequential placement — concurrent reviewers run in separate guests, so neither can see the other's review, and the config is rejected rather than silently falling back.

A guest that was asked to judge and could not writes the failure into its artifact instead of failing the job: two good reviews are worth more than the reconciliation, and Barry reconciles them itself with `[llm.judge]` if one is configured.

### Multi-Review Pipeline

1. **Persona drafts** — Each reviewer identity runs N persona prompts in parallel (security, correctness, style, rust). Each persona sees a filtered diff (e.g., Rust persona only sees `.rs` files).
2. **R1 synthesis** — Each identity synthesizes persona drafts into one unified review. Nothing is posted yet.
3. **R2 synthesis** — Each identity reads the other's R1 review and may revise.
4. **Judge** — The judge LLM receives both R2 reviews and decides `agree` or `disagree` with a reason.
5. **Posting** — Depending on the verdict: only Barry posts (agree), both post (disagree), or Barry posts alone if Other Barry was unreachable.

### Key Patterns

- **Single-actor SQLite** — All database access goes through a single blocking thread via message passing
- **Job queue** — Jobs are leased with a timeout; concurrent workers process without duplicates. Events for the same PR are coalesced.
- **Cancellation propagation** — `CancellationToken` per PR; when a PR closes, all in-flight checkers are cancelled between API calls

## Setup

### 1. Register Three GitHub Apps

Create three GitHub Apps, one per reviewer identity. Each needs the following permissions:

| Permission | Required For |
|---|---|
| Pull requests | Read & Write |
| Contents | Read |
| Checks | Write |
| Issues & PR comments | Write |

Only **Barry** subscribes to webhook events (`pull_request` and `issue_comment`). The other two are write-only identities that mint installation tokens to post reviews.

### 2. Install All Three Apps

Install all three on the repositories you want reviewed.

### 3. Generate Config

Copy `config/barry.toml.example` to `barry.toml` and fill in:

- Three App IDs and private key paths
- SQLite storage path
- LLM profiles for each identity and the judge
- Dispatcher settings (workers, timeouts, debounce)
- Confer rules (who may summon additional reviewers)

Optionally, copy `config/.barry.toml.example` to `.barry.toml` in any repo to customize behavior per-repository.

### 3b. Choose Which Repositories Barry Acts On

```toml
repos = ["you/project", "you/other-project"]
```

Absent means every repository the Apps are installed on. If you installed them
with "All repositories", that is every repository in the account — and since a
review costs real compute, "installed everywhere" and "reviews everywhere"
being the same set is a spending decision worth making deliberately.

This is a **spend gate, not an authorization boundary**. The installation is
the authorization boundary: an App installed on a repository can write to it
whatever this list says. Narrowing the installation is still the only thing
that reduces what the Apps can reach.

Checked before a delivery becomes a job, so an unlisted repository costs one
string compare rather than a worker and a review. Case-insensitive, as GitHub
is. Drops are counted as `barry_webhook_rejected_total{reason="repo"}`.

### 4. Set Environment Variables

```bash
export BARRY_WEBHOOK_SECRET=<your webhook secret>
export ANTHROPIC_API_KEY=<your key>      # if used by any [llm.*] profile
export OPENAI_API_KEY=<your key>         # if used by any [llm.*] profile
```

### 5. Start Barry

```bash
cargo run --release -- run --config barry.toml
```

### 6. Expose the Webhook

Barry's App receives webhooks on its configured endpoint. Two ways to get them there:

**Directly** — the App's webhook URL points at Barry, which is reachable from the internet.

**Through a relay** — the App's webhook URL is a [smee.io](https://smee.io) channel, and Barry holds that channel open from the inside:

```toml
[relay]
smee_url = "https://smee.io/aBcDeF123456"
```

Nothing inbound is needed, which is what makes Barry deployable on a rack behind a LAN. Two things to know about it:

- **smee keeps no backlog.** An event delivered while the relay is disconnected is gone, and GitHub still records a `200` from smee itself — so nothing goes red. `/healthz` reports the relay's connection state and how long ago it last saw an event; a missed delivery is redelivered by hand from the App's delivery log.
- **Signatures still verify end to end.** The relay re-POSTs to Barry's own `/webhook` carrying GitHub's `X-Hub-Signature-256`, so the HMAC check is not skipped. This works because the body is re-serialized byte-identically (serde_json's `preserve_order`); `barry_relay_rejected_total{reason="signature"}` counts it if that ever stops being true.

## Configuration

### Global Config (`barry.toml`)

| Section | Key | Description |
|---|---|---|
| `[server]` | `listen` | HTTP listen address (default: `0.0.0.0:8181`) |
| | `public_url` | Public URL for webhook registration (optional) |
| `[github.barry]` | `app_id` | Barry's GitHub App ID |
| | `private_key_path` | Path to Barry's PEM private key |
| | `webhook_secret_env` | Env var containing webhook HMAC secret |
| `[github.other_barry]` | `app_id` | Other Barry's GitHub App ID |
| | `private_key_path` | Path to Other Barry's PEM key |
| `[github.other_other_barry]` | `app_id` | Other Other Barry's GitHub App ID |
| | `private_key_path` | Path to Other Other Barry's PEM key |
| (top level) | `repos` | `owner/name` list barry acts on; absent means all |
| `[relay]` | `smee_url` | smee.io channel to hold open, when GitHub cannot reach Barry |
| | `require_signature` | Forward GitHub's signature (default `true`) rather than re-signing |
| `[storage]` | `sqlite_path` | Path to the SQLite database file |
| `[dispatcher]` | `debounce_secs` | Seconds to debounce synchronize events (default: 30) |
| | `worker_count` | Parallel worker pool size (default: 4) |
| | `job_timeout_secs` | Per-job timeout (default: 1800) |
| | `checker_timeout_secs` | Per-checker timeout (default: 600) |
| `[llm.*]` | `provider` | `"anthropic"` or `"openai"` |
| | `endpoint` | API base URL |
| | `api_key_env` | Env var for API key |
| | `model` | Model name |
| | `max_tokens` | Max output tokens |
| | `request_timeout_secs` | Request timeout |
| `[confer]` | `allowed` | Roles allowed to run `/barry confer` |
| | `max_per_pr` | Max additional reviewers per PR |
| `[personas.*]` | `prompt_path` | Optional custom prompt file override |

### Per-Repo Config (`.barry.toml`)

Fetched from the repo's HEAD at runtime, allows overriding hygiene rules and disabling multi-review per repository. See `config/.barry.toml.example` for format.

## Signals

| signal | effect |
|---|---|
| `SIGHUP` | **checks** the config on disk and reports whether it is valid. It does not apply it — nothing is rebuilt from a new config. Restart to apply a change. |
| `SIGTERM` | graceful shutdown: the HTTP server drains, workers finish the job in hand. |

## Slash Commands

| Command | Who | Behavior |
|---|---|---|
| `/barry approve` | Maintainer (write/maintain/admin) | Trusts the PR author, enabling automatic review on next PR event |
| `/barry review` | Maintainer | Re-runs the full review pipeline on the current head |
| `/barry confer` | Maintainer + PR author | Summons the next unposted reviewer (OB, then OOB) for an independent opinion |

## Security

- **HMAC-SHA256 verification** — Webhook signatures verified with constant-time compare (`subtle::ConstantTimeEq`) before any payload is parsed
- **Key file permissions** — Private key `.pem` files must be mode `0600` or stricter; Barry refuses to start otherwise
- **Trust gate** — Authors with read permission require `/barry approve` from a maintainer before review runs. Approval is sticky for the PR lifetime.
- **Endpoint validation** — `provider = "anthropic"` is rejected with non-anthropic endpoint hosts, preventing misconfiguration from leaking diffs to wrong LLM endpoints
- **Diff exposure** — Code diffs are sent to configured LLM endpoints. Do not run on repos containing secrets or PII that should not leave your environment

## Metrics

The `/metrics` endpoint exposes Prometheus metrics:

- `barry_multi_review_judge_total{verdict="agree"|"disagree"}`
- `barry_multi_review_barry_alone_total` — Other Barry was unreachable
- `barry_relay_connected` — 1 while the relay holds the channel open
- `barry_relay_events_total{event}`, `barry_relay_rejected_total{reason}`
- `barry_rack_judge_total{outcome="verdict"|"missing"|"invalid"}`
- `barry_confer_total{outcome="ob"|"oob"|"rejected_unauthorized"|"rejected_max_reached"|"rejected_no_run"|"rejected_all_posted"}`
- Job, webhook, and queue counters

## Build & Test

```bash
# Debug build
cargo build

# Release build
cargo build --release

# Run all tests
cargo test

# Run integration tests
cargo test --test integration
```

## Smoke Test

On a sandbox repo with all three Apps installed:

1. Open a PR. Confirm the four `barry/hygiene.*` Check Runs appear, plus `barry/llm-review`, and a review comment posted by **Barry** (or two reviews — one each from Barry and Other Barry — if the judge says they disagree).
2. Push more commits within 30s. Confirm only one extra run fires (debounce).
3. From a non-maintainer account, open a PR. Confirm only the "needs approval" comment appears. Comment `/barry approve` as a maintainer. Confirm the normal Check Runs and review now appear.
4. After a review has posted, comment `/barry confer`. Confirm Other Barry posts an independent review on the same head SHA. Comment `/barry confer` again — Other Other Barry posts. A third `/barry confer` is rejected with "Maximum confers reached" (default `max_per_pr = 2`).
5. Break `.barry.toml` (e.g. invalid TOML). Confirm a `barry/config` Check Run with `failure` appears, and other checkers do not run.
