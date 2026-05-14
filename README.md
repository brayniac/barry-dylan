# barry-bot

A GitHub App that runs automated PR review across one or a few organizations.
Single Rust binary, embedded SQLite, webhook-driven.

The "Barry & Other Barry" multi-reviewer feature gives each PR two independent
LLM reviewers (different model providers, different personas — security,
correctness, style — merged internally) plus a hidden judge that decides
whether they materially agree. When they agree, only Barry posts. When they
disagree, both post and the check-run goes neutral. A maintainer can summon a
third reviewer ("Other Other Barry") with `/barry confer`.

## Status

v1 ships:
- Multi-reviewer LLM review (two visible identities + hidden judge + optional confer)
- PR hygiene: title format, description, size warning, auto-labels
- Trust gate: untrusted PRs require `/barry approve` from a maintainer
- `/barry review`, `/barry approve`, `/barry confer` slash commands

Future: sandboxed lint/test/benchmark runners.

## Running

1. **Register three GitHub Apps**, one per reviewer identity (Barry, Other
   Barry, Other Other Barry). They'll appear as distinct users on the PR.
   Each App needs PR read/write, contents read, checks write, and
   issues/comments write. Only **Barry** subscribes to webhook events
   (`pull_request` and `issue_comment`) — OB and OOB are write-only.

2. **Install all three Apps** on the repos you want reviewed.

3. **Generate config files** — copy `config/barry.toml.example` to
   `barry.toml` and fill in the three App IDs, three private key paths, and
   SQLite path. Copy `config/.barry.toml.example` to `.barry.toml` in any
   repo you want to customize per-repo behavior in.

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

6. **Expose the webhook endpoint** (Barry's App only). For dev, use
   [smee.io](https://smee.io) or [ngrok](https://ngrok.com) to forward a
   public URL to `http://localhost:8181/webhook`.

## Slash commands

- `/barry approve` — maintainer-only; trusts the PR author for this PR so
  reviews start running. Sticky for the lifetime of the PR.
- `/barry review` — re-runs the full review pipeline on the current head.
- `/barry confer` — summons the next un-posted Barry (OB then OOB) to post
  an independent review on the current head SHA. Bounded by
  `[confer].max_per_pr` and gated by `[confer].allowed` roles.

## Smoke test

On a sandbox repo with all three Apps installed:

1. Open a PR. Confirm the four `barry/hygiene.*` Check Runs appear, plus
   `barry/llm-review`, and a review comment posted by **Barry** (or two
   reviews — one each from Barry and Other Barry — if the judge says they
   disagree).
2. Push more commits within 30s. Confirm only one extra run fires (debounce).
3. From a non-maintainer account, open a PR. Confirm only the "needs
   approval" comment appears. Comment `/barry approve` as a maintainer.
   Confirm the normal Check Runs and review now appear.
4. After a review has posted, comment `/barry confer`. Confirm Other Barry
   posts an independent review on the same head SHA. Comment `/barry confer`
   again — Other Other Barry posts. A third `/barry confer` is rejected
   with "Maximum confers reached" (default `max_per_pr = 2`).
5. Break `.barry.toml` (e.g. invalid TOML). Confirm a `barry/config` Check
   Run with `failure` appears, and other checkers do not run.

## Metrics

`/metrics` (Prometheus) exposes:
- `barry_multi_review_judge_total{verdict="agree"|"disagree"}`
- `barry_multi_review_barry_alone_total` — Other Barry was unreachable
- `barry_confer_total{outcome="ob"|"oob"|"rejected_unauthorized"|"rejected_max_reached"|"rejected_no_run"|"rejected_all_posted"}`
- plus the usual job/webhook counters.

## Security

- Diffs are sent to the configured LLM endpoints. Do not run on repos
  containing secrets/PII that should not leave your environment.
- Each App private key file must have mode `0600` or stricter; barry-bot
  refuses to start otherwise.
- The factory rejects `provider = "anthropic"` paired with a non-anthropic
  endpoint host — a misconfigured profile won't leak diffs to the wrong API.
- Webhooks are HMAC-SHA256 verified with constant-time compare before any
  payload is parsed.
