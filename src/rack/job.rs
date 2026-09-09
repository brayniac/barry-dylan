//! Building the systemslab experiment that produces the reviews.

use super::config::{Placement, RackConfig, Reviewer};
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
/// With more than one reviewer the models are served in turn: pull, serve,
/// review, stop, next. Each one gets the whole GPU, which is what makes two 14B
/// models on a 24 GB card possible at all.
pub fn payload(
    cfg: &RackConfig,
    reviewers: &[&Reviewer],
    files: &[ChangedFile],
) -> anyhow::Result<String> {
    let files_json = serde_json::to_string(files)?;
    let files_b64 = base64::engine::general_purpose::STANDARD.encode(files_json);

    let mut s = String::new();
    s.push_str(
        r#"set -euxo pipefail

sudo apt-get update -qq
sudo apt-get install -y barry-dylan
barry-dylan --version

"#,
    );
    s.push_str(&format!(
        "echo '{files_b64}' | base64 -d > /tmp/changed.json\n\n"
    ));

    for r in reviewers {
        s.push_str(&format!(
            r#"# ---- {identity} ----
rm -rf /tmp/model
slipway pull '{model}' /tmp/model
GGUF=$(find /tmp/model -name '*.gguf' | head -1)
test -n "$GGUF" || {{ echo "no gguf materialised from {model}"; exit 1; }}

llama-server -m "$GGUF" --host 127.0.0.1 --port 8080 -ngl 99 -c {context} \
    --no-webui > /tmp/llama-{identity}.log 2>&1 &
SERVER_PID=$!

# Bounded, and it gives up as soon as the server dies rather than waiting out
# the whole loop to report a timeout that was really a crash.
for _ in $(seq 1 240); do
    curl -sf http://127.0.0.1:8080/health >/dev/null 2>&1 && break
    kill -0 $SERVER_PID 2>/dev/null || {{ tail -40 /tmp/llama-{identity}.log; exit 1; }}
    sleep 2
done
curl -sf http://127.0.0.1:8080/health >/dev/null \
    || {{ tail -40 /tmp/llama-{identity}.log; exit 1; }}

cat > /tmp/offline-{identity}.toml <<'OFFLINE'
[llm]
provider = "openai"
endpoint = "http://127.0.0.1:8080/v1"
model = "{model_name}"
max_tokens = {max_tokens}
request_timeout_secs = 600
OFFLINE

barry-dylan review-offline \
    --config /tmp/offline-{identity}.toml \
    --files /tmp/changed.json \
    --out /tmp/{artifact}

# Free the whole card before the next model loads. Two 14B q4 models at this
# context do not fit on one 24 GB card together, and a partially offloaded
# second model would silently review at a fraction of the speed.
kill $SERVER_PID
wait $SERVER_PID 2>/dev/null || true

"#,
            identity = r.identity,
            model = r.model,
            model_name = r.model_name,
            context = cfg.context_size,
            max_tokens = cfg.max_tokens,
            artifact = r.artifact(),
        ));
    }

    Ok(s)
}

/// Build one job's steps for the given reviewers.
fn job_value(
    cfg: &RackConfig,
    reviewers: &[&Reviewer],
    shape: &str,
    host_tags: &[String],
    files: &[ChangedFile],
) -> anyhow::Result<Value> {
    let mut steps = vec![json!({
        "uses": "anvil-vm",
        "with": {
            "shape": shape,
            "image": cfg.image,
            "payload": payload(cfg, reviewers, files)?,
            "payload_timeout": cfg.payload_timeout_secs,
            "artifacts": reviewers
                .iter()
                .map(|r| format!("/tmp/{}", r.artifact()))
                .collect::<Vec<_>>(),
            // Guest-side telemetry around a GPU workload. The hypervisor cannot
            // see the GPU at all, so this is the only place it can come from.
            "metrics": true,
        }
    })];
    for r in reviewers {
        steps.push(json!({
            "uses": "upload-artifact",
            "with": { "path": r.artifact() }
        }));
    }
    steps.push(json!({
        "uses": "upload-artifact",
        "with": { "path": "rezolus.rez" }
    }));

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

    Ok(json!({
        "experiment": {
            "name": name,
            "jobs": jobs,
        }
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
                    .strip_suffix("' | base64 -d > /tmp/changed.json")
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
        assert_eq!(p.matches("base64 -d > /tmp/changed.json").count(), 1);
        assert_eq!(p.matches("--files /tmp/changed.json").count(), 2);
    }

    #[test]
    fn each_reviewer_gets_its_own_model_server_and_artifact() {
        let c = cfg();
        let p = payload(&c, &all(&c), &files()).unwrap();
        assert!(p.contains("--out /tmp/review-barry.json"));
        assert!(p.contains("--out /tmp/review-other_barry.json"));
        assert!(p.contains(&c.reviewers[0].model));
        assert!(p.contains(&c.reviewers[1].model));
    }

    #[test]
    fn the_server_is_stopped_between_reviewers() {
        // Two 14B q4 models do not fit on one 24 GB card together. Leaving the
        // first server up would push the second into partial offload, which is
        // not an error -- just a review that runs at a fraction of the speed.
        let c = cfg();
        let p = payload(&c, &all(&c), &files()).unwrap();
        assert_eq!(p.matches("kill $SERVER_PID").count(), 2);
    }

    #[test]
    fn sequential_puts_every_reviewer_in_one_job() {
        let c = cfg();
        let s = spec(&c, &files(), "t").unwrap();
        let jobs = s["experiment"]["jobs"].as_object().unwrap();
        assert_eq!(jobs.len(), 1);
        let step = &jobs["review"]["steps"][0];
        assert_eq!(step["with"]["artifacts"].as_array().unwrap().len(), 2);
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
            assert_eq!(
                job["steps"][0]["with"]["artifacts"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
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
}
