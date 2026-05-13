# barry-bot

A GitHub App that runs automated PR review (LLM) and PR hygiene checks across
one or a few organizations. Single Rust binary, embedded SQLite, webhook-driven.

## Status

v1 ships:
- LLM-powered review comments (inline)
- PR hygiene: title format, description, size warning, auto-labels
- Trust gate: untrusted PRs require `/barry approve` from a maintainer

Future: sandboxed lint/test/benchmark runners.

## Running

1. **Register a GitHub App** with the permissions listed in
   `docs/superpowers/specs/2026-05-12-barry-bot-design.md` and subscribe to
   `pull_request` and `issue_comment` events.

2. **Generate config files** — copy `config/barry.toml.example` to `barry.toml`
   and fill in your App ID, key path, and SQLite path. Copy
   `config/.barry.toml.example` to `.barry.toml` in any repo you want to
   customize per-repo behavior in.

3. **Set env vars**:
   ```bash
   export BARRY_WEBHOOK_SECRET=<your webhook secret>
   export ANTHROPIC_API_KEY=<your key>
   ```

4. **Start**:
   ```bash
   cargo run --release -- run --config barry.toml
   ```

5. **Expose the webhook endpoint**. For dev, use [smee.io](https://smee.io) or
   [ngrok](https://ngrok.com) to forward a public URL to `http://localhost:8080/webhook`.

## Smoke test

On a sandbox repo with the App installed:

1. Open a PR. Confirm:
   - A `barry/hygiene.title` Check Run appears.
   - A `barry/hygiene.description` Check Run appears.
   - A `barry/hygiene.size` Check Run appears.
   - A `barry/hygiene.autolabel` Check Run appears.
   - A `barry/llm-review` PR review appears with inline comments (if findings).
2. Push more commits within 30s. Confirm only one extra run fires (debounce).
3. From a non-maintainer account, open a PR. Confirm only the "needs approval"
   comment appears. Comment `/barry approve` as a maintainer. Confirm the
   normal Check Runs and review now appear.
4. Break `.barry.toml` (e.g. invalid TOML). Confirm a `barry/config` Check Run
   with `failure` appears, and other checkers do not run.

## Security

- Diffs are sent to the configured LLM endpoint. Do not run on repos containing
  secrets/PII that should not leave your environment.
- The App private key file must have mode `0600` or stricter; barry-bot
  refuses to start otherwise.
