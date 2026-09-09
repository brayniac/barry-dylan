use serde::Deserialize;

/// How to run a review on the rack instead of against a local endpoint.
///
/// A rack review is a systemslab experiment: it acquires a GPU host, creates an
/// ephemeral VM, serves a model in it, runs `barry-dylan review-offline`
/// inside, and returns the review as an artifact. Barry never talks to the
/// model — it asks for a review and gets one back.
///
/// The alternative, a resident `llama-server`, holds a GPU host out of the
/// measurement pool permanently. That is the trade this exists to avoid.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RackConfig {
    /// systemslab control plane, e.g. `http://systemslab`.
    pub systemslab: String,

    /// Host tags constraining which hypervisor the job may land on. These pick
    /// the *host*, and so must agree with `shape`: a `z2.` shape cannot run on
    /// a Zen1 host.
    pub host_tags: Vec<String>,

    /// anvil instance type, e.g. `z2.g.medium`. The `g` class is what gets a
    /// GPU passed through.
    pub shape: String,

    /// Guest image, e.g. `spool/images/debian-13-gpu@golden`.
    pub image: String,

    /// The reviewers to run. Order is the order they run in when sequential.
    pub reviewers: Vec<Reviewer>,

    /// Whether the reviewers share one guest or get one each.
    #[serde(default)]
    pub placement: Placement,

    /// llama-server context window. A review sends whole patches, so this wants
    /// to be generous.
    #[serde(default = "default_context_size")]
    pub context_size: u32,

    /// Cap on a single completion inside the guest.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,

    /// How often to ask the control plane whether the experiment has finished.
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,

    /// Give up on the experiment after this long. Covers instance creation, a
    /// multi-gigabyte model pull, model load, and the review itself.
    #[serde(default = "default_job_timeout_secs")]
    pub job_timeout_secs: u64,

    /// Bound on the payload inside the guest, handed to the `anvil-vm` action.
    /// Distinct from `job_timeout_secs`, which also covers scheduling: a job
    /// can sit queued behind another for a long time without its payload having
    /// started.
    #[serde(default = "default_payload_timeout_secs")]
    pub payload_timeout_secs: u64,
}

/// Where the reviewers run.
///
/// Measured on this rack: instance create to reachable is ~20 s, a 9 GB model
/// pull is ~79 s, and llama-server loads a 14B q4 in ~4 s from a warm page
/// cache. So the model *reload* between two reviewers is nearly free; the pull
/// is what costs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    /// One guest, models served in turn.
    ///
    /// The two pulls share one hypervisor's 1 GbE link, so they cannot overlap
    /// usefully -- 18 GB is ~167 s however it is split. In exchange this uses
    /// one GPU host and leaves the other free, which is the premise the whole
    /// VM-as-job design rests on.
    #[default]
    Sequential,
    /// One guest per reviewer, as separate jobs in the same experiment.
    ///
    /// systemslab schedules the jobs independently, so each reviewer lands on
    /// its own hypervisor and pulls over that host's own link -- roughly
    /// halving wall clock. The cost is that one review occupies both of the
    /// rack's GPUs.
    ///
    /// Each reviewer needs a `shape` and `host_tags` that agree with each
    /// other, since the two hypervisors are different generations: a `z2.`
    /// shape cannot run on the Zen1 host.
    ///
    /// Keeping the jobs in one experiment means one state to poll and
    /// all-or-nothing semantics, rather than two submissions that can disagree
    /// about whether the review happened.
    Concurrent,
}

/// One identity's review, and the model that produces it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reviewer {
    /// Which of barry's identities this review belongs to. Matches
    /// [`crate::checker::multi_review::identity::Identity`]'s slug.
    pub identity: String,

    /// slipway reference for the weights, e.g.
    /// `Qwen/Qwen2.5-Coder-14B-Instruct@latest/gguf/q4_k_m@latest`.
    pub model: String,

    /// What to call the model over the OpenAI-compatible API. llama-server
    /// serves one model and does not route on this, but it is echoed back and
    /// ends up in logs, so it should say something true.
    pub model_name: String,

    /// Instance type for this reviewer, overriding the top-level `shape`.
    /// Required in practice for `concurrent`, where the two reviewers land on
    /// hypervisors of different generations.
    #[serde(default)]
    pub shape: Option<String>,

    /// Host tags for this reviewer, overriding the top-level `host_tags`. Must
    /// agree with `shape`.
    #[serde(default)]
    pub host_tags: Option<Vec<String>>,
}

impl Reviewer {
    /// The artifact this reviewer's review is written to and collected as.
    pub fn artifact(&self) -> String {
        format!("review-{}.json", self.identity)
    }

    /// The systemslab job name carrying this reviewer, when concurrent.
    pub fn job_name(&self) -> String {
        format!("review-{}", self.identity)
    }

    pub fn shape<'a>(&'a self, cfg: &'a RackConfig) -> &'a str {
        self.shape.as_deref().unwrap_or(&cfg.shape)
    }

    pub fn host_tags<'a>(&'a self, cfg: &'a RackConfig) -> &'a [String] {
        self.host_tags.as_deref().unwrap_or(&cfg.host_tags)
    }
}

fn default_context_size() -> u32 {
    32768
}

fn default_max_tokens() -> u32 {
    4096
}

fn default_poll_interval_secs() -> u64 {
    10
}

fn default_job_timeout_secs() -> u64 {
    3600
}

fn default_payload_timeout_secs() -> u64 {
    3000
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
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

    #[test]
    fn a_minimal_config_parses_with_defaults() {
        let cfg: RackConfig = toml::from_str(MINIMAL).unwrap();
        assert_eq!(cfg.shape, "z2.g.medium");
        assert_eq!(cfg.context_size, 32768);
        assert_eq!(cfg.max_tokens, 4096);
        assert_eq!(cfg.poll_interval_secs, 10);
    }

    #[test]
    fn reviewers_keep_their_order_and_get_distinct_artifacts() {
        // Order is the order they run in the guest, and each writes its own
        // file -- one artifact reused would leave the second review silently
        // overwriting the first.
        let cfg: RackConfig = toml::from_str(MINIMAL).unwrap();
        assert_eq!(cfg.reviewers[0].identity, "barry");
        assert_eq!(cfg.reviewers[1].identity, "other_barry");
        assert_eq!(cfg.reviewers[0].artifact(), "review-barry.json");
        assert_eq!(cfg.reviewers[1].artifact(), "review-other_barry.json");
        assert_ne!(cfg.reviewers[0].artifact(), cfg.reviewers[1].artifact());
    }

    #[test]
    fn the_payload_timeout_is_shorter_than_the_job_timeout() {
        // The job timeout also covers time spent queued behind another job, so
        // it must leave room beyond the payload's own bound. If the payload
        // could outlive the job timeout, barry would give up on an experiment
        // that was still legitimately working.
        let cfg: RackConfig = toml::from_str(MINIMAL).unwrap();
        assert!(cfg.payload_timeout_secs < cfg.job_timeout_secs);
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        // A silently ignored key would be a review quietly running on the wrong
        // shape, or against the wrong model.
        let text = format!("{MINIMAL}\nmodle = \"typo\"\n");
        let err = toml::from_str::<RackConfig>(&text).unwrap_err().to_string();
        assert!(err.contains("modle"), "{err}");
    }

    #[test]
    fn a_missing_required_key_is_rejected() {
        let err = toml::from_str::<RackConfig>("systemslab = \"http://s\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("host_tags"), "{err}");
    }
}
