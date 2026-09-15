# Relay and rack judge — Design

Two changes that between them let barry-dylan run as a service on delta with no
inbound reachability and no remote LLM key:

1. **The relay** — barry holds an outbound connection to a smee.io channel and
   feeds what arrives to its own webhook endpoint. GitHub keeps posting to a
   public URL; the rack needs no tunnel, no domain, and no forwarded port.
2. **The rack judge** — the judge runs in the same ephemeral GPU guest that
   produced the two reviews, against the model already loaded there, instead of
   against a remote endpoint. `[llm.judge]` becomes optional.

## Why now

barry ran from a laptop between 2026-05-14 and 2026-05-23 — 1,116 audit rows
across five repositories — with `smee` forwarding to `localhost:8181`. It has
been down since. The apps were never uninstalled: all three are still installed
on `brayniac` with `repository_selection = all`, and GitHub is still delivering
(`200` from smee, which accepts and drops when no client is connected).

So nothing about *GitHub* is blocking. What is blocking is that the deployment
target — delta — cannot receive a webhook, and the config the installer writes
still needs a remote LLM key for the judge.

### Why not polling

rack-ci, on the same host and for the same reason, polls instead of receiving
(`infra/docs/guides/ci.md`, "Why it polls"). That was the obvious precedent and
it is the wrong one here:

- barry's slash commands (`/barry approve`, `/barry review`, `/barry confer`)
  arrive as `issue_comment`. Polling them means listing comments on every open
  PR every sweep plus a last-seen-comment-id per PR. Without it the trust gate
  has no escape hatch: an untrusted author's PR can never be approved.
- A review is GPU-minutes, so "build anything unremembered" on restart is far
  more expensive here than rack-ci's equivalent, and needs rack-ci's whole
  `decide()`/first-sweep apparatus to avoid.
- smee already works, is already configured in the app, and costs no code in
  the trigger path.

Polling stays a live option — nothing here forecloses it — but it is a day of
Rust to replace something that already works.

## 1. The relay

### Shape

```
GitHub --POST--> smee.io/<channel> --SSE--> barry (outbound) --POST--> 127.0.0.1:8181/webhook
```

`[relay]` absent means off, so an existing config behaves exactly as it does
today and nothing starts by surprise.

```toml
[relay]
smee_url = "https://smee.io/b3Enu1k5QSBsAYY"
# Reject an event whose HMAC does not verify after re-serialization. Default
# true; see "The signature problem".
require_signature = true
```

### Why re-POST rather than call the handler in-process

Calling `webhook::parse` directly would skip HMAC verification — in the one
component that most needs it. A smee channel is an unauthenticated public
endpoint: anyone who learns the URL can post to it. Re-POSTing to barry's own
`/webhook` keeps one intake path and one verification path, and the relay ends
up trusted for *transport only*.

### The signature problem

smee does not forward raw bytes. It parses the JSON body and re-emits it inside
its own envelope, so the exact bytes GitHub signed are gone by the time they
arrive. The HMAC only verifies if re-serialization is byte-identical.

`serde_json::Value` sorts object keys (it is backed by `BTreeMap`), which would
fail *every* signature. The relay therefore requires serde_json's
`preserve_order` feature; with it, a compact re-serialization matches GitHub's
compact payload in the ordinary case.

"Ordinary case" is doing real work in that sentence, so:

- `barry_relay_signature_mismatch_total` counts every rejection, so a systematic
  break is visible immediately rather than as silence.
- `require_signature = false` is the escape hatch, documented as what it is: the
  relay becomes the trust boundary, and the channel URL becomes a secret.

### Reconnection and visibility

smee keeps **no backlog**. An event delivered while the relay is disconnected is
gone, and GitHub still records `200`, so nothing anywhere goes red. That makes
liveness the whole game:

- reconnect with capped exponential backoff, logging each transition;
- `/healthz` reports relay state (`connected`, last event age), so
  `infra diff` and a human both have something to look at;
- `barry_relay_events_total`, `barry_relay_connects_total`,
  `barry_relay_disconnects_total`.

Recovery for a missed event is manual redelivery from the app's delivery log,
which is a documented limitation, not a bug to fix later.

### Module

`src/relay.rs`, spawned from `app_runtime::run` when `[relay]` is present:

- `parse_frame(&str) -> Option<SmeeEvent>` — pure, over an SSE frame body.
  Ignores smee's `ping` events and its `ready` handshake.
- `headers_for(&SmeeEvent) -> HeaderMap` — reconstructs `X-GitHub-Event`,
  `X-GitHub-Delivery`, `X-Hub-Signature-256`.
- `body_bytes(&SmeeEvent) -> Vec<u8>` — compact re-serialization.
- `run(cfg, shutdown)` — connect, stream, dispatch, reconnect.

`reqwest` already carries the `stream` feature and `futures` is already a
dependency, so this adds no crate but `serde_json/preserve_order`.

## 2. The rack judge

### What the guest already does

`rack::job` builds a systemslab experiment whose guest installs barry-dylan from
the rack's internal apt repo, starts `llama-server` per reviewer (one server when
both reviewers name the same model, which is delta's configuration), runs
`barry-dylan review-offline` per reviewer, and uploads each review as an
artifact. delta downloads the artifacts and calls `judge_reviews()` locally
against `[llm.judge]`.

The model is loaded, the reviews are on disk, and the guest is about to be
destroyed — which is the moment the judge's work is cheapest.

### Change

- **New subcommand** `barry-dylan judge-offline --a <review.json> --b <review.json>
  --config <offline.toml> --out <verdict.json>`, mirroring `review-offline`:
  no GitHub, no storage, one model endpoint.
- **`rack::job`** appends a judge step after the review steps, against the
  server already running, and uploads `verdict.json`.
- **`rack::review`** returns the verdict alongside the reviews when the artifact
  is present.
- **`orchestrator`** uses that verdict when it exists and otherwise calls
  `judge_reviews()` exactly as today.

### Placement

Only `Placement::Sequential` can do this: it is the arrangement where one guest
holds both reviews. Under `Concurrent` the reviews are produced in different
guests, and moving them around to reconcile costs more than the judge saves.

Config validation therefore **rejects** `judge = true` with
`placement = "concurrent"` rather than silently falling back — a silent fallback
here means paying for a remote judge you thought you had stopped paying for.

```toml
[rack]
judge = true          # default false: today's behaviour
```

### Degradation

A rack judge that fails is not fatal, and for the same reason the local one
isn't: the orchestrator already treats a failed judge as "post Barry alone".
A missing verdict artifact falls back to `[llm.judge]` if configured, and to
Barry-alone if not. Both paths are counted:
`barry_rack_judge_total{outcome="verdict"|"missing"|"invalid"}`.

### What this buys

With `judge = true` and `[rack]` reviewers, barry needs **no LLM credential at
all**: no `ANTHROPIC_API_KEY`, no diff leaving the rack. The only secret left in
`secrets.env` is `BARRY_WEBHOOK_SECRET`.

## 3. Packaging and deployment

### The repository joins rack-ci

barry-dylan has no `.rack-ci.toml` today. It gets one, plus a `.rack-release.sh`
modelled on anvil's: build, `cargo deb`, leave the package in `dist/`, and let
the `[release]` job publish it to delta's internal apt repo.

This matters more here than for most repos: **the rack guest installs
barry-dylan from that apt repo**, so `judge-offline` must be published before
any rack job can call it. The ordering is: publish the deb, then upgrade delta,
then turn the relay on.

Two steps are the rack owner's and cannot be done from the repository
(`use-rack-ci`, step 4): adding `brayniac/barry-dylan` to `repos` in
`/etc/rack-ci/rack-ci.toml` with `[overrides."brayniac/barry-dylan"] release = true`,
and ticking the repository in the fine-grained token.

### infra

- `host-setup/install-barry-dylan`: the starter config gains `[relay]` and
  `[rack] judge = true`, and loses its Anthropic instructions.
- `fleet/hosts/delta.toml`: `barry-dylan` joins `services`, its version moves to
  the released one, and the "deliberately absent" note at the end goes — it was
  true only while the service could not start.

### Blast radius, first

All three apps are installed on `brayniac` with `repository_selection = all` —
**189 repositories**. barry has no repo allowlist (the dispatcher does not filter;
the installation is the authorization boundary), so starting the relay against
that install means reviewing PR activity across all 189, at GPU-minutes each, on
the two hypervisors that also run measurements.

The installations are narrowed to selected repositories **before** the relay is
enabled. That is a GitHub UI action — `all` → `selected` is not an API
operation. Starting set: the five barry already knows (slipway, ferallm2,
ferallm, barry-dylan, levinson).

## Testing

Unit, inline, named as claims:

- `a_ping_frame_is_not_an_event`
- `a_relayed_body_keeps_githubs_key_order`
- `an_event_whose_signature_does_not_verify_is_rejected`
- `a_disconnect_reconnects_with_backoff`
- `judge_true_with_concurrent_placement_is_a_config_error`
- `the_judge_step_runs_against_the_server_already_started`
- `a_missing_verdict_artifact_falls_back_to_the_local_judge`

Integration: an axum test server standing in for smee, asserting an event
survives the round trip to a mounted `/webhook` with its signature intact.

## Out of scope

- GitHub polling (see "Why not polling").
- A repo allowlist in barry's own config. The installation is the boundary
  today; adding a second one is a separate decision.
- Replacing smee with self-hosted relay infrastructure. If smee proves unreliable
  the answer is probably polling, not a tunnel.
