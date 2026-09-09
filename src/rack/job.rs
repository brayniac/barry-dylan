//! Building the systemslab experiment that produces a review.

use super::config::RackConfig;
use crate::github::pr::ChangedFile;
use base64::Engine as _;
use serde_json::{Value, json};

/// The artifact the guest writes and barry reads back.
pub const REVIEW_ARTIFACT: &str = "review.json";

/// Render the bash the guest runs.
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
pub fn payload(cfg: &RackConfig, files: &[ChangedFile]) -> anyhow::Result<String> {
    let files_json = serde_json::to_string(files)?;
    let files_b64 = base64::engine::general_purpose::STANDARD.encode(files_json);

    Ok(format!(
        r#"set -euxo pipefail

sudo apt-get update -qq
sudo apt-get install -y barry-dylan
barry-dylan --version

slipway pull '{model}' /tmp/model
GGUF=$(find /tmp/model -name '*.gguf' | head -1)
test -n "$GGUF" || {{ echo "no gguf materialised from {model}"; exit 1; }}

llama-server -m "$GGUF" --host 127.0.0.1 --port 8080 -ngl 99 -c {context} \
    --no-webui > /tmp/llama-server.log 2>&1 &
SERVER_PID=$!
trap 'kill $SERVER_PID 2>/dev/null' EXIT

# Bounded, and it gives up as soon as the server dies rather than waiting out
# the whole loop to report a timeout that was really a crash.
for _ in $(seq 1 240); do
    curl -sf http://127.0.0.1:8080/health >/dev/null 2>&1 && break
    kill -0 $SERVER_PID 2>/dev/null || {{ tail -40 /tmp/llama-server.log; exit 1; }}
    sleep 2
done
curl -sf http://127.0.0.1:8080/health >/dev/null || {{ tail -40 /tmp/llama-server.log; exit 1; }}

cat > /tmp/offline.toml <<'OFFLINE'
[llm]
provider = "openai"
endpoint = "http://127.0.0.1:8080/v1"
model = "{model_name}"
max_tokens = {max_tokens}
request_timeout_secs = 600
OFFLINE

echo '{files_b64}' | base64 -d > /tmp/changed.json

barry-dylan review-offline \
    --config /tmp/offline.toml \
    --files /tmp/changed.json \
    --out /tmp/{artifact}
"#,
        model = cfg.model,
        model_name = cfg.model_name,
        context = cfg.context_size,
        max_tokens = cfg.max_tokens,
        files_b64 = files_b64,
        artifact = REVIEW_ARTIFACT,
    ))
}

/// The experiment body posted to `/api/v1/submit`.
///
/// The native JSON form, not the TOML one: systemslab's TOML dialect has a
/// closed set of step types and cannot name a custom action like `anvil-vm` at
/// all.
pub fn spec(cfg: &RackConfig, files: &[ChangedFile], name: &str) -> anyhow::Result<Value> {
    Ok(json!({
        "experiment": {
            "name": name,
            "jobs": {
                "review": {
                    "host": { "tags": cfg.host_tags },
                    "steps": [
                        {
                            "uses": "anvil-vm",
                            "with": {
                                "shape": cfg.shape,
                                "image": cfg.image,
                                "payload": payload(cfg, files)?,
                                "payload_timeout": cfg.payload_timeout_secs,
                                "artifacts": [format!("/tmp/{REVIEW_ARTIFACT}")],
                                // Guest-side telemetry around a GPU workload.
                                // The hypervisor cannot see the GPU at all, so
                                // this is the only place it can come from.
                                "metrics": true,
                            }
                        },
                        {
                            "uses": "upload-artifact",
                            "with": { "path": REVIEW_ARTIFACT }
                        },
                        {
                            "uses": "upload-artifact",
                            "with": { "path": "rezolus.rez" }
                        }
                    ]
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> RackConfig {
        toml::from_str(
            r#"
systemslab = "http://systemslab"
host_tags = ["z2.baremetal"]
shape = "z2.g.medium"
image = "spool/images/debian-13-gpu@golden"
model = "Qwen/Qwen2.5-Coder-14B-Instruct@latest/gguf/q4_k_m@latest"
model_name = "qwen2.5-coder-14b"
"#,
        )
        .unwrap()
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

    #[test]
    fn the_payload_carries_the_files_base64_encoded() {
        // A patch contains quotes, dollar signs, backslashes and newlines. Any
        // of those interpolated raw would either break the shell or silently
        // corrupt the diff the model reviews.
        let p = payload(&cfg(), &files()).unwrap();
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
    fn the_payload_writes_the_artifact_the_spec_collects() {
        // These two agreeing is the whole contract between the guest and barry.
        let p = payload(&cfg(), &files()).unwrap();
        assert!(p.contains(&format!("--out /tmp/{REVIEW_ARTIFACT}")));

        let s = spec(&cfg(), &files(), "t").unwrap();
        let step = &s["experiment"]["jobs"]["review"]["steps"][0];
        assert_eq!(
            step["with"]["artifacts"][0],
            format!("/tmp/{REVIEW_ARTIFACT}")
        );
        let upload = &s["experiment"]["jobs"]["review"]["steps"][1];
        assert_eq!(upload["with"]["path"], REVIEW_ARTIFACT);
    }

    #[test]
    fn the_payload_fails_the_job_rather_than_reviewing_nothing() {
        // Without `set -e` and the explicit checks, a failed model pull would
        // leave llama-server serving nothing and the review would be produced
        // against an endpoint that never answered.
        let p = payload(&cfg(), &files()).unwrap();
        assert!(p.contains("set -euxo pipefail"));
        assert!(p.contains("no gguf materialised"));
    }

    #[test]
    fn config_values_reach_the_guest() {
        let mut c = cfg();
        c.context_size = 8192;
        c.max_tokens = 512;
        let p = payload(&c, &files()).unwrap();
        assert!(p.contains("-c 8192"), "context size not applied");
        assert!(p.contains("max_tokens = 512"), "max tokens not applied");
        assert!(p.contains(&c.model), "model ref not applied");
        assert!(p.contains(&c.model_name), "model name not applied");
    }

    #[test]
    fn the_spec_asks_for_the_host_the_shape_can_run_on() {
        let s = spec(&cfg(), &files(), "review pr 1").unwrap();
        assert_eq!(s["experiment"]["name"], "review pr 1");
        assert_eq!(
            s["experiment"]["jobs"]["review"]["host"]["tags"][0],
            "z2.baremetal"
        );
        assert_eq!(
            s["experiment"]["jobs"]["review"]["steps"][0]["with"]["shape"],
            "z2.g.medium"
        );
    }
}
