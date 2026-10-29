//! Building the systemslab experiment that produces the reviews.

use super::config::{JUDGE_LOG, Placement, RackConfig, Reviewer, VERDICT_ARTIFACT};
use crate::github::pr::ChangedFile;
use base64::Engine as _;
use serde_json::{Value, json};

/// Render the bash one guest runs for the reviewers assigned to it.
///
/// The inputs travel inside the payload rather than as pushed files: the
/// offline config is a few lines, and the changed-file set is base64 so nothing
/// in a patch has to survive shell or JSON quoting. A very large PR would want
/// a systemslab context artifact instead, but that trades a second round trip
/// and a lifecycle for a problem this does not have yet.
///
/// barry-dylan is installed at run time from the rack's internal apt repo
/// rather than baked into the guest image. The image carries stable
/// infrastructure — driver, CUDA, llama-server, slipway — and rebuilding it is
/// a 25-minute, 7.5 GB operation, while barry ships far more often than that.
///
/// In a guest, with more than one reviewer, the models are served in turn:
/// pull, serve, review, stop, next, and reviewers sharing a model share its
/// server. Each one gets the whole GPU, which is what makes two 14B models on a
/// 24 GB card possible at all.
///
/// On a bare host every reviewer, and the judge, gets a server started for it
/// alone and stopped after it. The server runs in the job's process group, so
/// the runner's kill on a cancel or timeout takes it down with the job.
pub fn payload(
    cfg: &RackConfig,
    reviewers: &[&Reviewer],
    files: &[ChangedFile],
) -> anyhow::Result<String> {
    let files_json = serde_json::to_string(files)?;
    let files_b64 = base64::engine::general_purpose::STANDARD.encode(files_json);

    let mut s = String::new();
    // barry-dylan, plus whatever the review needs to run at all. The gpu image
    // carries the platform and not the workload: it has the driver, CUDA and
    // rezolus, and deliberately neither llama-server (llama.cpp moves far too
    // fast for a baked version to mean anything) nor the slipway client. A
    // payload that assumed them failed at `slipway: command not found` after
    // the 2026-09-12 image rebuild, having proved only that apt works.
    //
    // A bare job works in its own job directory, which systemslab removes, so
    // nothing is left in the host's /tmp; a guest is destroyed whole.
    s.push_str(&format!(
        r#"set -euxo pipefail
T={dir}

sudo apt-get update -qq
sudo apt-get install -y barry-dylan{packages}
barry-dylan --version
llama-server --version 2>&1 | head -1 || true

SERVER_PID=
# SIGTERM, then SIGKILL after 10 s: a degraded server can take minutes to stop.
stop_server() {{
    if [ -n "$SERVER_PID" ]; then
        kill "$SERVER_PID" 2>/dev/null || true
        for _ in $(seq 1 20); do
            kill -0 "$SERVER_PID" 2>/dev/null || break
            sleep 0.5
        done
        kill -9 "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
        SERVER_PID=
    fi
}}
trap stop_server EXIT

"#,
        dir = if cfg.bare { "\"$PWD\"" } else { "/tmp" },
        packages = cfg
            .packages
            .iter()
            .map(|p| format!(" {p}"))
            .collect::<String>(),
    ));
    s.push_str(&format!(
        "echo '{files_b64}' | base64 -d > \"$T/changed.json\"\n\n"
    ));

    let server_args: String = cfg.server_args.iter().map(|a| format!(" {a}")).collect();
    // Serve `r`'s model, logging to `log`, and wait for it to answer. `fail`
    // ends the script (`exit 1`) or, inside a function, reports to the caller
    // (`return 1`); every step that can fail names it, since `set -e` does not
    // apply inside a function called as an `if` condition.
    let start = |r: &Reviewer, log: &str, fail: &str| -> String {
        let mut out = String::new();
        if r.is_local() {
            out.push_str(&format!(
                "GGUF={model}\ntest -r \"$GGUF\" || {{ echo \"cannot read $GGUF\"; {fail}; }}\n",
                model = r.model,
            ));
        } else {
            out.push_str(&format!(
                r#"rm -rf "$T/model"
slipway pull '{model}' "$T/model" || {{ echo "the model pull failed"; {fail}; }}
GGUF=$(find "$T/model" -name '*.gguf' | head -1) || {fail}
test -n "$GGUF" || {{ echo "no gguf materialised from {model}"; {fail}; }}
"#,
                model = r.model,
            ));
        }
        out.push_str(&format!(
            r#"
# Something already answering on the port would be reviewed against in place
# of the server this job starts.
if curl -sf http://127.0.0.1:8080/health >/dev/null 2>&1; then
    echo "port 8080 is already serving; refusing to review against it"; {fail}
fi
llama-server -m "$GGUF" --host 127.0.0.1 --port 8080 -ngl 99 -c {context} \
    --no-webui{server_args} > "$T/{log}" 2>&1 &
SERVER_PID=$!

# Bounded, and it gives up as soon as the server dies rather than waiting out
# the whole loop to report a timeout that was really a crash.
for _ in $(seq 1 240); do
    kill -0 "$SERVER_PID" 2>/dev/null || {{ tail -40 "$T/{log}"; {fail}; }}
    curl -sf http://127.0.0.1:8080/health >/dev/null 2>&1 && break
    sleep 2
done
curl -sf http://127.0.0.1:8080/health >/dev/null \
    || {{ tail -40 "$T/{log}"; {fail}; }}

"#,
            context = r.context_size(cfg),
        ));
        out
    };
    let stop = "stop_server\n\n";

    for (i, r) in reviewers.iter().enumerate() {
        // In a guest, reviewers sharing a model share the server. Two
        // identities on the same weights is a normal configuration -- the
        // diversity is then in the sampling rather than the model -- and
        // pulling 16 GB twice and reloading it to serve the identical file
        // would be pure waste. On a bare host every reviewer gets its own.
        let start_server = cfg.bare || i == 0 || reviewers[i - 1].model != r.model;
        let stop_server = cfg.bare
            || reviewers
                .get(i + 1)
                .is_none_or(|next| next.model != r.model);

        s.push_str(&format!("# ---- {} ----\n", r.identity));
        if start_server {
            s.push_str(&start(r, &r.server_log(), "exit 1"));
        }

        s.push_str(&format!(
            r#"cat > "$T/offline-{identity}.toml" <<'OFFLINE'
[llm]
provider = "openai"
endpoint = "http://127.0.0.1:8080/v1"
model = "{model_name}"
max_tokens = {max_tokens}
context_size = {context_size}
{options}request_timeout_secs = {request_timeout}
OFFLINE

# The server log is an artifact whether or not this reviewer started the
# server (a reviewer sharing a model shares the server, and its log). Touched
# so that the upload never fails on a file that was never written.
touch "$T/llama-{identity}.log"
barry-dylan review-offline \
    --config "$T/offline-{identity}.toml" \
    --files "$T/changed.json" \
    --out "$T/{artifact}"

"#,
            identity = r.identity,
            model_name = r.model_name,
            max_tokens = cfg.max_tokens,
            context_size = r.context_size(cfg),
            options = profile_options(cfg),
            request_timeout = cfg.request_timeout_secs,
            artifact = r.artifact(),
        ));

        // The judge runs in here, not on delta, when asked to -- after the last
        // review. It reconciles two finished reviews, which is one completion,
        // so it costs the job seconds and saves delta a remote model and the
        // credential that comes with it. In a guest it uses the server already
        // loaded, before that server is stopped; on a bare host it gets a
        // server of its own, as the reviewers do.
        //
        // A judge that fails must not lose two good reviews, so its failure is
        // written into the artifact rather than raised: `set -e` would fail the
        // whole job, throwing away the reviews to save the reconciliation.
        // delta sees a verdict it cannot parse and reconciles the reviews
        // itself.
        let last = i + 1 == reviewers.len();
        let judging = cfg.judge && last && reviewers.len() >= 2;
        // On a bare host the judge's server is started in a function, so a
        // server that does not come up costs the verdict, not the two reviews.
        if judging && cfg.bare {
            s.push_str(stop);
            s.push_str("# ---- judge ----\njudge_server() {\n");
            s.push_str(&start(r, JUDGE_LOG, "return 1"));
            s.push_str(&format!(
                "}}\nif ! judge_server; then\n    echo '{{\"error\":\"the judge server did not start\"}}' > \"$T/{VERDICT_ARTIFACT}\"\nelse\n"
            ));
        }
        if judging {
            let a = reviewers[0];
            let b = reviewers[1];
            s.push_str(&format!(
                r#"cat > "$T/offline-judge.toml" <<'OFFLINE'
[llm]
provider = "openai"
endpoint = "http://127.0.0.1:8080/v1"
model = "{model_name}"
max_tokens = {judge_max_tokens}
{options}request_timeout_secs = {request_timeout}
OFFLINE

barry-dylan judge-offline \
    --config "$T/offline-judge.toml" \
    --a "$T/{a_artifact}" \
    --b "$T/{b_artifact}" \
    --out "$T/{verdict}" \
    || echo '{{"error":"judge-offline failed in the guest"}}' > "$T/{verdict}"
"#,
                model_name = r.model_name,
                judge_max_tokens = cfg.judge_max_tokens,
                options = profile_options(cfg),
                request_timeout = cfg.request_timeout_secs,
                a_artifact = a.artifact(),
                b_artifact = b.artifact(),
                verdict = VERDICT_ARTIFACT,
            ));
            s.push_str(if cfg.bare { "fi\n\n" } else { "\n" });
        }

        if stop_server || judging {
            if !cfg.bare {
                s.push_str(
                    r#"# Free the whole card before the next model loads. Two large q4 models at
# this context do not fit on one 24 GB card together, and a partially offloaded
# second model would silently review at a fraction of the speed.
"#,
                );
            }
            s.push_str(stop);
        }
    }

    Ok(s)
}

/// The optional lines of a guest-side `[llm]` profile: `temperature`,
/// `thinking` and `structured_output`, each only when `[rack]` sets it.
/// Absent temperature means llama-server uses the model's own recommendation.
fn profile_options(cfg: &RackConfig) -> String {
    let mut s = String::new();
    if let Some(t) = cfg.temperature {
        s.push_str(&format!("temperature = {t}\n"));
    }
    if let Some(t) = cfg.thinking {
        s.push_str(&format!("thinking = {t}\n"));
    }
    if let Some(o) = cfg.structured_output {
        s.push_str(&format!("structured_output = {o}\n"));
    }
    s
}

/// Build one job's steps for the given reviewers.
fn job_value(
    cfg: &RackConfig,
    reviewers: &[&Reviewer],
    shape: &str,
    host_tags: &[String],
    files: &[ChangedFile],
) -> anyhow::Result<Value> {
    // Only the job that holds every reviewer can judge, which under sequential
    // placement is the only job there is.
    let judging = cfg.judge && reviewers.len() >= 2;
    // A bare job writes its artifacts straight into the job directory, where
    // the upload steps look; a guest's are pulled out by the action.
    let first = if cfg.bare {
        json!({ "uses": "shell", "with": { "run": payload(cfg, reviewers, files)? } })
    } else {
        json!({
        "uses": "anvil-vm",
        "with": {
            "shape": shape,
            "image": cfg.image,
            "payload": payload(cfg, reviewers, files)?,
            "payload_timeout": cfg.payload_timeout_secs,
            "artifacts": reviewers
                .iter()
                .map(|r| format!("/tmp/{}", r.artifact()))
                .chain(reviewers.iter().map(|r| format!("/tmp/{}", r.server_log())))
                .chain(judging.then(|| format!("/tmp/{VERDICT_ARTIFACT}")))
                .collect::<Vec<_>>(),
            // Guest-side telemetry around a GPU workload. The hypervisor cannot
            // see the GPU at all, so this is the only place it can come from.
            "metrics": true,
        }
        })
    };
    let mut steps = vec![first];
    for r in reviewers {
        steps.push(json!({
            "uses": "upload-artifact",
            "with": { "path": r.artifact() }
        }));
        // The llama-server log answers the questions a failed or odd review
        // raises -- slot count, context per slot, what the model was really
        // served with -- and until 2026-09-18 it died with the guest.
        steps.push(json!({
            "uses": "upload-artifact",
            "with": { "path": r.server_log() }
        }));
    }
    if judging {
        steps.push(json!({
            "uses": "upload-artifact",
            "with": { "path": VERDICT_ARTIFACT }
        }));
        if cfg.bare {
            steps.push(json!({
                "uses": "upload-artifact",
                "with": { "path": JUDGE_LOG }
            }));
        }
    }
    if !cfg.bare {
        steps.push(json!({
            "uses": "upload-artifact",
            "with": { "path": "rezolus.rez" }
        }));
    }

    Ok(json!({
        "host": { "tags": host_tags },
        "steps": steps,
    }))
}

/// The experiment body posted to `/api/v1/submit`.
///
/// The native JSON form, not the TOML one: systemslab's TOML dialect has a
/// closed set of step types and cannot name a custom action like `anvil-vm` at
/// all.
///
/// Concurrent placement puts each reviewer in its own job of the *same*
/// experiment, so there is still one state to poll and the whole thing succeeds
/// or fails together — rather than two submissions that can disagree about
/// whether the review happened.
pub fn spec(cfg: &RackConfig, files: &[ChangedFile], name: &str) -> anyhow::Result<Value> {
    if cfg.reviewers.is_empty() {
        anyhow::bail!("no reviewers configured; nothing to run");
    }
    cfg.validate().map_err(|e| anyhow::anyhow!(e))?;

    let mut jobs = serde_json::Map::new();
    match cfg.placement {
        Placement::Sequential => {
            let all: Vec<&Reviewer> = cfg.reviewers.iter().collect();
            jobs.insert(
                "review".to_string(),
                job_value(cfg, &all, &cfg.shape, &cfg.host_tags, files)?,
            );
        }
        Placement::Concurrent => {
            for r in &cfg.reviewers {
                jobs.insert(
                    r.job_name(),
                    job_value(cfg, &[r], r.shape(cfg), r.host_tags(cfg), files)?,
                );
            }
        }
    }

    // `priority` is a sibling of the experiment in the submit body, not a key
    // inside it: systemslab's `SubmitRequest` is `{experiment, context,
    // priority}`, and a priority written inside the experiment is ignored.
    Ok(json!({
        "experiment": {
            "name": name,
            "jobs": jobs,
        },
        "priority": cfg.priority,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
systemslab = "http://systemslab"
host_tags = ["z2.baremetal"]
shape = "z2.g.medium"
image = "spool/images/debian-13-gpu@golden"

[[reviewers]]
identity = "barry"
model = "Qwen/Qwen2.5-Coder-14B-Instruct@latest/gguf/q4_k_m@latest"
model_name = "qwen2.5-coder-14b"

[[reviewers]]
identity = "other_barry"
model = "meta-llama/Llama-3.1-8B@latest/gguf/q5_k_m@latest"
model_name = "llama-3.1-8b"
"#;

    fn cfg() -> RackConfig {
        toml::from_str(BASE).unwrap()
    }

    fn concurrent_cfg() -> RackConfig {
        // `placement` must precede the [[reviewers]] tables: a top-level key
        // written after them belongs to the last table, so appending it makes
        // it an unknown reviewer field rather than a placement.
        toml::from_str(&format!("placement = \"concurrent\"\n{BASE}")).unwrap()
    }

    fn files() -> Vec<ChangedFile> {
        vec![ChangedFile {
            filename: "src/a.rs".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 0,
            changes: 1,
            patch: Some("@@ -1 +1,2 @@\n fn a() {}\n+// 'quotes' and $vars\n".into()),
        }]
    }

    fn all(cfg: &RackConfig) -> Vec<&Reviewer> {
        cfg.reviewers.iter().collect()
    }

    fn judging_cfg() -> RackConfig {
        toml::from_str(&format!("judge = true\n{BASE}")).unwrap()
    }

    #[test]
    fn the_guest_installs_what_the_review_needs_to_run() {
        // The image has neither, by design, and the payload is the only place
        // that can put them there.
        let c = cfg();
        let p = payload(&c, &all(&c), &files()).unwrap();
        let install = p
            .lines()
            .find(|l| l.contains("apt-get install"))
            .expect("no install line");
        assert!(install.contains("barry-dylan"), "{install}");
        assert!(install.contains("slipway"), "{install}");
        assert!(install.contains("llama-server"), "{install}");
    }

    #[test]
    fn pinned_packages_reach_the_guest_as_written() {
        let mut c = cfg();
        c.packages = vec!["slipway=0.2.3-1".into(), "llama-server=10502-1".into()];
        let p = payload(&c, &all(&c), &files()).unwrap();
        assert!(
            p.contains("barry-dylan slipway=0.2.3-1 llama-server=10502-1"),
            "{p}"
        );
    }

    #[test]
    fn a_package_spec_that_is_really_a_shell_fragment_is_refused() {
        // It is interpolated into bash. `validate` is the only thing between a
        // config file and arbitrary commands in a guest.
        let mut c = cfg();
        c.packages = vec!["slipway; rm -rf /".into()];
        let err = spec(&c, &files(), "t").unwrap_err().to_string();
        assert!(err.contains("plain apt spec"), "unexpected error: {err}");
    }

    #[test]
    fn the_judge_runs_before_the_server_it_uses_is_stopped() {
        // The judge is cheap only because the weights are already resident.
        // Emitted after the last `kill`, it would be talking to a port with
        // nothing behind it.
        let c = judging_cfg();
        let p = payload(&c, &all(&c), &files()).unwrap();
        let judge = p.find("judge-offline").expect("no judge step");
        let last_kill = p.rfind("\nstop_server\n").expect("no server stop");
        assert!(
            judge < last_kill,
            "the judge runs after the last server is stopped"
        );
    }

    #[test]
    fn the_judge_reconciles_both_reviewers_artifacts() {
        let c = judging_cfg();
        let p = payload(&c, &all(&c), &files()).unwrap();
        assert!(p.contains("--a \"$T/review-barry.json\""), "{p}");
        assert!(p.contains("--b \"$T/review-other_barry.json\""), "{p}");
    }

    #[test]
    fn the_judge_gets_a_budget_a_reasoning_model_can_answer_within() {
        // 512 was the in-process judge's cap, sized for a small remote model.
        // A local model that reasons first spent all of it thinking and
        // returned nothing, so the first real rack judge produced no verdict.
        let c = judging_cfg();
        let p = payload(&c, &all(&c), &files()).unwrap();
        let judge_cfg = p
            .split("offline-judge.toml")
            .nth(1)
            .expect("no judge config");
        assert!(judge_cfg.contains("max_tokens = 4096"), "{judge_cfg}");
    }

    #[test]
    fn a_judge_that_fails_does_not_fail_the_job() {
        // `set -e` plus a bare judge command would throw away two finished
        // reviews to save the reconciliation, which is exactly backwards.
        let c = judging_cfg();
        let p = payload(&c, &all(&c), &files()).unwrap();
        assert!(
            p.contains(r#"|| echo '{"error":"judge-offline failed in the guest"}'"#),
            "{p}"
        );
    }

    #[test]
    fn no_judge_step_without_the_flag() {
        let c = cfg();
        let p = payload(&c, &all(&c), &files()).unwrap();
        assert!(!p.contains("judge-offline"));
    }

    #[test]
    fn the_verdict_is_collected_and_uploaded() {
        let c = judging_cfg();
        let spec = spec(&c, &files(), "t").unwrap();
        let job = &spec["experiment"]["jobs"]["review"];
        let artifacts = job["steps"][0]["with"]["artifacts"].as_array().unwrap();
        assert!(
            artifacts.iter().any(|a| a == "/tmp/verdict.json"),
            "verdict not pulled out of the guest: {artifacts:?}"
        );
        let uploads: Vec<_> = job["steps"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s["with"]["path"].as_str())
            .collect();
        assert!(uploads.contains(&"verdict.json"), "{uploads:?}");
    }

    #[test]
    fn judging_with_concurrent_placement_is_refused() {
        // Each reviewer is alone in its own guest there, so neither can see the
        // other's review. Refused rather than quietly reconciled on delta: the
        // reason to set this is to stop paying a remote judge, and a silent
        // fallback keeps paying one.
        let mut c = concurrent_cfg();
        c.judge = true;
        let err = spec(&c, &files(), "t").unwrap_err().to_string();
        assert!(err.contains("sequential"), "unexpected error: {err}");
    }

    #[test]
    fn judging_one_reviewer_is_refused() {
        let mut c = cfg();
        c.judge = true;
        c.reviewers.truncate(1);
        let err = spec(&c, &files(), "t").unwrap_err().to_string();
        assert!(err.contains("two reviewers"), "unexpected error: {err}");
    }

    #[test]
    fn the_payload_carries_the_files_base64_encoded() {
        // A patch contains quotes, dollar signs, backslashes and newlines. Any
        // of those interpolated raw would either break the shell or silently
        // corrupt the diff the model reviews.
        let c = cfg();
        let p = payload(&c, &all(&c), &files()).unwrap();
        assert!(!p.contains("'quotes' and $vars"), "patch was inlined raw");

        let b64 = p
            .lines()
            .find_map(|l| {
                l.strip_prefix("echo '")?
                    .strip_suffix("' | base64 -d > \"$T/changed.json\"")
            })
            .expect("no base64 line");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        let round: Vec<ChangedFile> = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(round[0].patch, files()[0].patch);
    }

    #[test]
    fn the_diff_is_decoded_once_for_every_reviewer() {
        // Both reviewers see the same bytes. Decoding per reviewer would be
        // harmless but decoding per reviewer *differently* would not be, and
        // one shared file makes that impossible.
        let c = cfg();
        let p = payload(&c, &all(&c), &files()).unwrap();
        assert_eq!(p.matches("base64 -d > \"$T/changed.json\"").count(), 1);
        assert_eq!(p.matches("--files \"$T/changed.json\"").count(), 2);
    }

    #[test]
    fn each_reviewer_gets_its_own_model_server_and_artifact() {
        let c = cfg();
        let p = payload(&c, &all(&c), &files()).unwrap();
        assert!(p.contains("--out \"$T/review-barry.json\""));
        assert!(p.contains("--out \"$T/review-other_barry.json\""));
        assert!(p.contains(&c.reviewers[0].model));
        assert!(p.contains(&c.reviewers[1].model));
    }

    #[test]
    fn reviewers_sharing_a_model_share_the_server() {
        // Two identities on the same weights is a normal configuration -- the
        // diversity is then in the sampling rather than the model. Pulling
        // 16 GB twice and reloading it to serve the identical file would be
        // pure waste, and the pull is the expensive part of a rack review.
        let c: RackConfig = toml::from_str(
            r#"
systemslab = "http://systemslab"
host_tags = ["z2.baremetal"]
shape = "z2.g.medium"
image = "img"

[[reviewers]]
identity = "barry"
model = "Qwen/Qwen3.5-27B@latest/gguf/q4_k_m@latest"
model_name = "qwen3.5-27b"

[[reviewers]]
identity = "other_barry"
model = "Qwen/Qwen3.5-27B@latest/gguf/q4_k_m@latest"
model_name = "qwen3.5-27b"
"#,
        )
        .unwrap();
        let p = payload(&c, &all(&c), &files()).unwrap();

        assert_eq!(p.matches("slipway pull").count(), 1, "pulled twice");
        assert_eq!(p.matches("llama-server -m").count(), 1, "served twice");
        assert_eq!(p.matches("\nstop_server\n").count(), 1, "stopped twice");
        // Both reviews still happen, and still into their own artifacts.
        assert_eq!(p.matches("review-offline").count(), 2);
        assert!(p.contains("--out \"$T/review-barry.json\""));
        assert!(p.contains("--out \"$T/review-other_barry.json\""));
    }

    #[test]
    fn the_server_is_stopped_between_reviewers() {
        // Two 14B q4 models do not fit on one 24 GB card together. Leaving the
        // first server up would push the second into partial offload, which is
        // not an error -- just a review that runs at a fraction of the speed.
        let c = cfg();
        let p = payload(&c, &all(&c), &files()).unwrap();
        assert_eq!(p.matches("\nstop_server\n").count(), 2);
    }

    #[test]
    fn sequential_puts_every_reviewer_in_one_job() {
        let c = cfg();
        let s = spec(&c, &files(), "t").unwrap();
        let jobs = s["experiment"]["jobs"].as_object().unwrap();
        assert_eq!(jobs.len(), 1);
        let step = &jobs["review"]["steps"][0];
        // A review and a server log per reviewer.
        assert_eq!(step["with"]["artifacts"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn concurrent_puts_each_reviewer_in_its_own_job() {
        // Separate jobs so systemslab schedules them onto different hosts, but
        // one experiment so there is a single state to poll.
        let c = concurrent_cfg();
        let s = spec(&c, &files(), "t").unwrap();
        let jobs = s["experiment"]["jobs"].as_object().unwrap();
        assert_eq!(jobs.len(), 2);
        assert!(jobs.contains_key("review-barry"));
        assert!(jobs.contains_key("review-other_barry"));
        for (_, job) in jobs {
            // A review and a server log for the one reviewer in this job.
            assert_eq!(
                job["steps"][0]["with"]["artifacts"]
                    .as_array()
                    .unwrap()
                    .len(),
                2
            );
        }
    }

    #[test]
    fn a_reviewer_can_override_shape_and_tags_for_the_other_generation() {
        // The two hypervisors are different generations, so a concurrent pair
        // cannot share one shape.
        let c: RackConfig = toml::from_str(
            r#"
systemslab = "http://systemslab"
host_tags = ["z2.baremetal"]
shape = "z2.g.medium"
image = "img"
placement = "concurrent"

[[reviewers]]
identity = "barry"
model = "m1"
model_name = "n1"

[[reviewers]]
identity = "other_barry"
model = "m2"
model_name = "n2"
shape = "z1.g.medium"
host_tags = ["z1.baremetal"]
"#,
        )
        .unwrap();
        let s = spec(&c, &files(), "t").unwrap();
        let jobs = &s["experiment"]["jobs"];
        assert_eq!(
            jobs["review-barry"]["steps"][0]["with"]["shape"],
            "z2.g.medium"
        );
        assert_eq!(jobs["review-barry"]["host"]["tags"][0], "z2.baremetal");
        assert_eq!(
            jobs["review-other_barry"]["steps"][0]["with"]["shape"],
            "z1.g.medium"
        );
        assert_eq!(
            jobs["review-other_barry"]["host"]["tags"][0],
            "z1.baremetal"
        );
    }

    #[test]
    fn the_payload_fails_the_job_rather_than_reviewing_nothing() {
        // Without `set -e` and the explicit checks, a failed model pull would
        // leave llama-server serving nothing and the review would be produced
        // against an endpoint that never answered.
        let c = cfg();
        let p = payload(&c, &all(&c), &files()).unwrap();
        assert!(p.contains("set -euxo pipefail"));
        assert_eq!(p.matches("no gguf materialised").count(), 2);
    }

    #[test]
    fn config_values_reach_the_guest() {
        let mut c = cfg();
        c.context_size = 8192;
        c.max_tokens = 512;
        let p = payload(&c, &all(&c), &files()).unwrap();
        assert_eq!(p.matches("-c 8192").count(), 2);
        assert_eq!(p.matches("max_tokens = 512").count(), 2);
        // review-offline needs the window too, to run the personas in turns
        // that fit it together.
        assert_eq!(p.matches("context_size = 8192").count(), 2);
    }

    #[test]
    fn a_reviewer_can_have_its_own_context_window() {
        // gemma-4-31B fits the card at 32k beside a 27B at 64k.
        let mut c = cfg();
        c.context_size = 65536;
        c.reviewers[1].context_size = Some(32768);
        let p = payload(&c, &all(&c), &files()).unwrap();
        assert_eq!(p.matches("-c 65536").count(), 1, "{p}");
        assert_eq!(p.matches("-c 32768").count(), 1, "{p}");
        assert_eq!(p.matches("context_size = 65536").count(), 1);
        assert_eq!(p.matches("context_size = 32768").count(), 1);
    }

    #[test]
    fn temperature_is_left_to_the_model_unless_set() {
        let c = cfg();
        let p = payload(&c, &all(&c), &files()).unwrap();
        assert!(!p.contains("temperature"), "{p}");

        let mut c = judging_cfg();
        c.temperature = Some(0.7);
        let p = payload(&c, &all(&c), &files()).unwrap();
        // Both reviewers and the judge.
        assert_eq!(p.matches("temperature = 0.7").count(), 3, "{p}");
    }

    #[test]
    fn the_server_logs_come_back_as_artifacts() {
        let c = cfg();
        let s = spec(&c, &files(), "t").unwrap();
        let s = s.to_string();
        assert!(s.contains("/tmp/llama-barry.log"), "{s}");
        assert!(s.contains("/tmp/llama-other_barry.log"), "{s}");
        let p = payload(&c, &all(&c), &files()).unwrap();
        assert!(p.contains("touch \"$T/llama-barry.log\""), "{p}");
        assert!(p.contains("touch \"$T/llama-other_barry.log\""), "{p}");
    }

    #[test]
    fn a_review_is_submitted_as_advisory_priority() {
        // 300 per infra's priorities guide: ahead of an unmarked benchmark,
        // behind a claimed measurement and behind gating CI. Sent where
        // systemslab reads it, beside the experiment rather than inside it.
        let s = spec(&cfg(), &files(), "t").unwrap();
        assert_eq!(s["priority"], 300);
        assert!(s["experiment"].get("priority").is_none());
    }

    #[test]
    fn the_priority_is_configurable() {
        let mut c = cfg();
        c.priority = 200;
        let s = spec(&c, &files(), "t").unwrap();
        assert_eq!(s["priority"], 200);
    }

    #[test]
    fn no_reviewers_is_an_error_not_an_empty_experiment() {
        let c: RackConfig = toml::from_str(
            r#"
systemslab = "http://s"
host_tags = ["t"]
shape = "s"
image = "i"
reviewers = []
"#,
        )
        .unwrap();
        assert!(spec(&c, &files(), "t").is_err());
    }

    const BARE: &str = r#"
systemslab = "http://systemslab"
host_tags = ["host:hv03"]
bare = true
judge = true
packages = []
server_args = ["-np", "1", "--cache-ram", "0", "--jinja"]

[[reviewers]]
identity = "barry"
model = "/var/lib/models/qwen3.6-35b-a3b/Qwen3.6-35B-A3B-Q4_K_M.gguf"
model_name = "qwen3.6-35b-a3b"

[[reviewers]]
identity = "other_barry"
model = "/var/lib/models/qwen3.6-35b-a3b/Qwen3.6-35B-A3B-Q4_K_M.gguf"
model_name = "qwen3.6-35b-a3b"
"#;

    fn bare_cfg() -> RackConfig {
        toml::from_str(BARE).unwrap()
    }

    /// `bash -n` on a payload: the payload is generated text, and a quoting
    /// slip is a job that fails at its first line on the rack.
    fn parses_as_bash(p: &str) {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("payload.sh");
        std::fs::write(&f, p).unwrap();
        let out = std::process::Command::new("bash")
            .arg("-n")
            .arg(&f)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}\n{p}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn a_guest_works_in_tmp_where_its_artifacts_are_pulled_from() {
        let c = judging_cfg();
        let p = payload(&c, &all(&c), &files()).unwrap();
        assert!(p.contains("\nT=/tmp\n"), "{p}");
        let s = spec(&c, &files(), "x").unwrap();
        let pulled = s["experiment"]["jobs"]["review"]["steps"][0]["with"]["artifacts"]
            .as_array()
            .unwrap();
        assert!(
            pulled
                .iter()
                .all(|a| a.as_str().unwrap().starts_with("/tmp/")),
            "{pulled:?}"
        );
        parses_as_bash(&p);
    }

    #[test]
    fn a_bare_review_is_one_shell_step_on_the_host() {
        let c = bare_cfg();
        let s = spec(&c, &files(), "x").unwrap();
        let job = &s["experiment"]["jobs"]["review"];
        assert_eq!(job["host"]["tags"], json!(["host:hv03"]));
        let steps = job["steps"].as_array().unwrap();
        assert_eq!(steps[0]["uses"], json!("shell"));
        assert!(steps.iter().all(|st| st["uses"] != json!("anvil-vm")));
        // No guest, so no guest metrics to collect.
        assert!(
            steps
                .iter()
                .all(|st| st["with"]["path"] != json!("rezolus.rez")),
            "{steps:?}"
        );
        assert!(
            steps
                .iter()
                .any(|st| st["with"]["path"] == json!(VERDICT_ARTIFACT))
        );
        parses_as_bash(steps[0]["with"]["run"].as_str().unwrap());
    }

    #[test]
    fn a_bare_review_gives_every_reviewer_and_the_judge_a_fresh_server() {
        let c = bare_cfg();
        let p = payload(&c, &all(&c), &files()).unwrap();
        assert!(p.contains("\nT=\"$PWD\"\n"), "{p}");
        // Barry, Other Barry and the judge: three servers, though they share
        // a model, each stopped before the next starts.
        assert_eq!(p.matches("llama-server -m").count(), 3, "{p}");
        assert_eq!(p.matches("\nstop_server\n").count(), 3, "{p}");
        assert!(p.contains("> \"$T/llama-judge.log\""), "{p}");
        let judge = p.find("judge-offline").unwrap();
        let judge_start = p.rfind("llama-server -m").unwrap();
        assert!(judge_start < judge, "{p}");
        // Each start refuses a port something else already answers on.
        assert_eq!(p.matches("refusing to review against it").count(), 3, "{p}");
        // In the job's process group, stopped on any exit.
        assert!(p.contains("trap stop_server EXIT"), "{p}");
        assert!(!p.contains("systemd-run"), "{p}");
        assert!(p.contains("--no-webui -np 1 --cache-ram 0 --jinja"), "{p}");
    }

    #[test]
    fn a_bare_config_may_not_install_the_cuda_llama_server() {
        let mut c = bare_cfg();
        c.packages = vec!["slipway".into(), "llama-server=10502-1".into()];
        assert!(c.validate().unwrap_err().contains("llama-server"));
        c.packages = vec!["slipway".into()];
        assert!(c.validate().is_ok());
    }

    #[test]
    fn a_bare_config_runs_its_reviewers_in_one_job() {
        let mut c = bare_cfg();
        c.judge = false;
        c.placement = crate::rack::config::Placement::Concurrent;
        assert!(c.validate().is_err());
    }

    #[test]
    fn other_other_barry_does_not_make_a_pair_to_judge() {
        let mut c = bare_cfg();
        c.reviewers[1].identity = "other_other_barry".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn a_local_model_is_served_without_a_pull() {
        let c = bare_cfg();
        let p = payload(&c, &all(&c), &files()).unwrap();
        assert!(!p.contains("slipway pull"), "{p}");
        assert!(
            p.contains("GGUF=/var/lib/models/qwen3.6-35b-a3b/Qwen3.6-35B-A3B-Q4_K_M.gguf"),
            "{p}"
        );
    }

    #[test]
    fn only_a_bare_config_may_leave_out_shape_and_image() {
        let mut c = bare_cfg();
        assert!(c.validate().is_ok());
        c.bare = false;
        assert!(c.validate().is_err());
    }

    #[test]
    fn server_args_and_local_paths_must_be_plain_words() {
        let mut c = bare_cfg();
        c.server_args = vec!["--jinja; rm -rf /".into()];
        assert!(c.validate().is_err());
        let mut c = bare_cfg();
        c.reviewers[0].model = "/var/lib/models/a b.gguf".into();
        assert!(c.validate().is_err());
    }

    /// Run the bare payload under bash with stubs for everything outside it.
    /// The stub server answers health checks while its marker file exists,
    /// fails to start on start number `fail_on` (0: never), and ignores
    /// SIGTERM when `deaf` is set. Returns the exit status and the directory.
    fn run_stubbed(fail_on: u32, deaf: bool) -> (std::process::Output, tempfile::TempDir) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let stub = |name: &str, body: &str| {
            let f = bin.join(name);
            std::fs::write(&f, format!("#!/bin/bash\n{body}\n")).unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        stub("sudo", r#"exec "$@""#);
        stub("apt-get", "exit 0");
        stub(
            "barry-dylan",
            r#"[ "$1" = --version ] && { echo barry-dylan 0; exit 0; }
while [ $# -gt 0 ]; do [ "$1" = --out ] && out=$2; shift; done
echo '{"outcome":"approve","summary":"s","findings":[]}' > "$out""#,
        );
        stub(
            "llama-server",
            &format!(
                r#"[ "$1" = --version ] && {{ echo version; exit 0; }}
n=$(( $(cat "$PWD/starts" 2>/dev/null || echo 0) + 1 )); echo $n > "$PWD/starts"
[ {fail_on} -eq $n ] && exit 1
{trap}
echo $$ > "$PWD/up"
while :; do sleep 0.1; done"#,
                trap = if deaf {
                    r#"trap '' TERM"#
                } else {
                    r#"trap 'rm -f "$PWD/up"; exit 0' TERM"#
                },
            ),
        );
        // A port answers while the process that bound it is alive.
        stub(
            "curl",
            r#"[ -e "$PWD/up" ] && kill -0 "$(cat "$PWD/up")" 2>/dev/null"#,
        );
        let model = dir.path().join("m.gguf");
        std::fs::write(&model, b"").unwrap();
        let mut c = bare_cfg();
        for r in c.reviewers.iter_mut() {
            r.model = model.display().to_string();
        }
        // The stop window shortened from 10 s to 1 s, so the test that waits it
        // out three times does not hold CI for half a minute.
        let p = payload(&c, &all(&c), &files()).unwrap().replacen(
            "for _ in $(seq 1 20); do",
            "for _ in $(seq 1 2); do",
            1,
        );
        let script = dir.path().join("payload.sh");
        std::fs::write(&script, p).unwrap();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        let out = std::process::Command::new("bash")
            .arg(&script)
            .current_dir(dir.path())
            .env("PATH", path)
            .output()
            .unwrap();
        (out, dir)
    }

    #[test]
    fn a_judge_server_that_does_not_start_costs_the_verdict_not_the_reviews() {
        // Barry's server is start 1, Other Barry's 2, the judge's 3.
        let (out, dir) = run_stubbed(3, false);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(dir.path().join("review-barry.json").exists());
        assert!(dir.path().join("review-other_barry.json").exists());
        let verdict = std::fs::read_to_string(dir.path().join(VERDICT_ARTIFACT)).unwrap();
        assert!(
            verdict.contains("the judge server did not start"),
            "{verdict}"
        );
    }

    #[test]
    fn a_server_that_ignores_sigterm_is_killed_and_the_job_finishes() {
        let started = std::time::Instant::now();
        let (out, dir) = run_stubbed(0, true);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(dir.path().join(VERDICT_ARTIFACT).exists());
        // Three servers, each given 1 s (in this test) to stop before SIGKILL.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_bare_judge_uploads_its_server_log() {
        let s = spec(&bare_cfg(), &files(), "x").unwrap();
        let steps = s["experiment"]["jobs"]["review"]["steps"]
            .as_array()
            .unwrap();
        assert!(
            steps
                .iter()
                .any(|st| st["with"]["path"] == json!(JUDGE_LOG)),
            "{steps:?}"
        );
    }

    #[test]
    fn thinking_and_structured_output_reach_every_offline_profile() {
        // Barry, Other Barry and the judge, on a bare host and in a guest.
        for mut c in [bare_cfg(), judging_cfg()] {
            c.thinking = Some(false);
            c.structured_output = Some(true);
            let p = payload(&c, &all(&c), &files()).unwrap();
            assert_eq!(p.matches("thinking = false\n").count(), 3, "{p}");
            assert_eq!(p.matches("structured_output = true\n").count(), 3, "{p}");
        }
        let unset = payload(&bare_cfg(), &all(&bare_cfg()), &files()).unwrap();
        assert!(!unset.contains("thinking ="), "{unset}");
    }

    /// The profile written into the payload must parse as the offline
    /// config's `[llm]` table, or `review-offline` fails at its first line.
    #[test]
    fn the_offline_profile_with_every_option_parses() {
        let mut c = bare_cfg();
        c.thinking = Some(false);
        c.structured_output = Some(true);
        c.temperature = Some(0.2);
        let text = format!(
            "[llm]\nprovider = \"openai\"\nendpoint = \"http://x/v1\"\nmodel = \"m\"\nmax_tokens = 1\n{}request_timeout_secs = 1\n",
            profile_options(&c)
        );
        let p: crate::offline::config::OfflineConfig = toml::from_str(&text).unwrap();
        assert_eq!(p.llm.thinking, Some(false));
        assert_eq!(p.llm.structured_output, Some(true));
    }
}
