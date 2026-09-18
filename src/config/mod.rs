use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub github: GitHubConfig,
    pub storage: StorageConfig,
    #[serde(default)]
    pub llm: std::collections::BTreeMap<String, LlmProfile>,
    pub dispatcher: DispatcherConfig,
    #[serde(default)]
    pub confer: ConferConfig,
    #[serde(default)]
    pub personas: PersonaOverridesConfig,
    #[serde(default)]
    pub defaults: Option<crate::config::repo::RepoConfig>,
    /// When present, the reviewer identities produce their reviews on the rack
    /// -- an ephemeral GPU VM per review -- instead of calling an LLM endpoint
    /// directly. The judge is unaffected: it reconciles two finished reviews
    /// and needs no GPU.
    #[serde(default)]
    pub rack: Option<crate::rack::RackConfig>,
    /// Repositories barry will act on, as `owner/name`.
    ///
    /// Absent means every repository the Apps are installed on, which is what
    /// barry did before this existed.
    ///
    /// This is a SPEND gate, not an authorization boundary, and the difference
    /// matters. The installation is the authorization boundary: an App
    /// installed on a repository can write to it whatever this list says. What
    /// the list bounds is what barry chooses to *do* -- and a review is
    /// GPU-minutes on a hypervisor that is also where measurements run, so
    /// "installed on 189 repositories" and "reviews 189 repositories" being the
    /// same number is a spending decision nobody made.
    ///
    /// Narrowing the installation is still worth doing; it is the only thing
    /// that reduces what the Apps can reach.
    #[serde(default)]
    pub repos: Option<Vec<String>>,

    /// GitHub logins who may command barry from a pull request comment: any
    /// `/barry` command, or `@barry-dylan`, which means `/barry review`. The
    /// comment's author is what GitHub signed into the delivery, so this is a
    /// real "who", not a string in the body.
    ///
    /// Absent means nobody. Until 2026-09-18 anyone who could comment on a
    /// pull request could re-run a review, which is a GPU host for forty
    /// minutes on a stranger's say-so.
    ///
    /// A commander's comment also gets through `repos`: it is one review of
    /// the head as it stands, in a repository barry does not otherwise act on.
    /// The repository stays unlisted -- its pushes, closes and everyone else's
    /// comments are still dropped -- so a second push needs a second ask.
    #[serde(default)]
    pub commands_from: Option<Vec<String>>,

    /// When present, barry holds a smee.io channel open and feeds what arrives
    /// to its own webhook endpoint. Absent means barry is reachable directly,
    /// which is true of a laptop with a tunnel and not of this rack.
    #[serde(default)]
    pub relay: Option<crate::relay::RelayConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub listen: String,
    #[serde(default)]
    pub public_url: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GitHubConfig {
    pub barry: IdentityCreds,
    pub other_barry: IdentityCreds,
    pub other_other_barry: IdentityCreds,
}

#[derive(Debug, Deserialize, Clone)]
pub struct IdentityCreds {
    pub app_id: u64,
    pub private_key_path: PathBuf,
    /// Webhook secret env var. Only Barry's identity needs this populated;
    /// OB/OOB do not receive webhooks.
    #[serde(default)]
    pub webhook_secret_env: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct StorageConfig {
    pub sqlite_path: PathBuf,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LlmProfile {
    pub provider: LlmProviderKind,
    pub endpoint: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    pub model: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_llm_timeout")]
    pub request_timeout_secs: u64,
    /// The context window of the model behind `endpoint`, in tokens, when it
    /// is known. A local llama-server shares one window across every request
    /// in flight, so the personas, which run concurrently, must together fit
    /// in it: prompt plus `max_tokens` each. Absent means "no idea", which is
    /// true of a hosted API, and the personas run all at once as before.
    #[serde(default)]
    pub context_size: Option<u32>,
}

/// Also the fallback for an identity with no profile at all, which is only
/// reachable with `[rack]` configured.
pub const DEFAULT_MAX_TOKENS: u32 = 8192;

fn default_max_tokens() -> u32 {
    DEFAULT_MAX_TOKENS
}
fn default_llm_timeout() -> u64 {
    300
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LlmProviderKind {
    Anthropic,
    Openai,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DispatcherConfig {
    #[serde(default = "default_debounce")]
    pub debounce_secs: u64,
    #[serde(default = "default_workers")]
    pub worker_count: usize,
    #[serde(default = "default_job_timeout")]
    pub job_timeout_secs: u64,
    #[serde(default = "default_checker_timeout")]
    pub checker_timeout_secs: u64,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        Self {
            debounce_secs: default_debounce(),
            worker_count: default_workers(),
            job_timeout_secs: default_job_timeout(),
            checker_timeout_secs: default_checker_timeout(),
        }
    }
}

fn default_debounce() -> u64 {
    30
}
fn default_workers() -> usize {
    4
}
fn default_job_timeout() -> u64 {
    1800
}
fn default_checker_timeout() -> u64 {
    600
}

#[derive(Debug, Deserialize, Clone)]
pub struct ConferConfig {
    #[serde(default = "default_allowed_roles")]
    pub allowed: Vec<String>,
    #[serde(default = "default_max_confers")]
    pub max_per_pr: u32,
}

impl Default for ConferConfig {
    fn default() -> Self {
        Self {
            allowed: default_allowed_roles(),
            max_per_pr: default_max_confers(),
        }
    }
}

fn default_allowed_roles() -> Vec<String> {
    vec![
        "author".into(),
        "write".into(),
        "maintain".into(),
        "admin".into(),
    ]
}
fn default_max_confers() -> u32 {
    2
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct PersonaOverridesConfig {
    #[serde(default)]
    pub security: Option<PersonaOverride>,
    #[serde(default)]
    pub correctness: Option<PersonaOverride>,
    #[serde(default)]
    pub style: Option<PersonaOverride>,
    #[serde(default)]
    pub rust: Option<PersonaOverride>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PersonaOverride {
    #[serde(default)]
    pub prompt_path: Option<PathBuf>,
}

pub mod repo;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading config file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("validation: {0}")]
    Validate(String),
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.into(),
            source: e,
        })?;
        let cfg: Config = toml::from_str(&text).map_err(|e| ConfigError::Parse {
            path: path.into(),
            source: e,
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(repos) = &self.repos {
            if repos.is_empty() {
                return Err(ConfigError::Validate(
                    "`repos` is present but empty, which would ignore every repository; \
                     remove the key to act on all of them"
                        .into(),
                ));
            }
            // A bare name never matches anything, and the way it fails is
            // silence -- barry receives the delivery and drops it. Caught here
            // instead, where it is one line of output.
            for r in repos {
                if r.split('/').filter(|p| !p.is_empty()).count() != 2 {
                    return Err(ConfigError::Validate(format!(
                        "`repos` entry {r:?} is not `owner/name`"
                    )));
                }
            }
        }
        if let Some(logins) = &self.commands_from {
            // GitHub logins have no `@` and no whitespace. A login written the
            // way it is typed in a comment would never match the way it is
            // delivered, and the way that fails is a command that is silently
            // dropped.
            for l in logins {
                if l.is_empty() || l.starts_with('@') || l.chars().any(char::is_whitespace) {
                    return Err(ConfigError::Validate(format!(
                        "`commands_from` entry {l:?} is not a bare GitHub login"
                    )));
                }
            }
        }
        if let Some(rack) = &self.rack {
            rack.validate().map_err(ConfigError::Validate)?;
        }

        // Without [rack], every identity needs an endpoint before anything can
        // run, and finding that out at startup is much better than finding it
        // out on someone's pull request.
        //
        // With [rack], the reviews come from a guest, so the endpoints are not
        // wanted at all -- and requiring them would mean a rack deployment
        // still had to carry a remote model's address and key to start. A
        // profile that is genuinely needed later (Other Other Barry, when
        // somebody confers) fails then, naming itself.
        let required: &[&str] = match &self.rack {
            None => &["barry", "other_barry", "other_other_barry", "judge"],
            Some(rack) if rack.judge => &[],
            // The rack produces reviews but was not asked to reconcile them,
            // so the judge is still this process's job.
            Some(_) => &["judge"],
        };
        for name in required {
            if !self.llm.contains_key(*name) {
                return Err(ConfigError::Validate(format!(
                    "an [llm.{name}] profile is required"
                )));
            }
        }
        if self.dispatcher.worker_count == 0 {
            return Err(ConfigError::Validate(
                "dispatcher.worker_count must be > 0".into(),
            ));
        }
        if self.github.barry.webhook_secret_env.is_none() {
            return Err(ConfigError::Validate(
                "[github.barry].webhook_secret_env is required (Barry receives webhooks)".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f
    }

    /// A config with [rack] and no [llm.*] at all. This is delta's shape, and
    /// before the rack judge existed it could not start: validate demanded four
    /// profiles for endpoints a rack deployment never calls.
    const RACK_ONLY: &str = r#"
[server]
listen = "0.0.0.0:8181"

[github.barry]
app_id = 1
private_key_path = "/tmp/b.pem"
webhook_secret_env = "WS"
[github.other_barry]
app_id = 2
private_key_path = "/tmp/ob.pem"
[github.other_other_barry]
app_id = 3
private_key_path = "/tmp/oob.pem"

[storage]
sqlite_path = "/tmp/b.db"

[dispatcher]

[rack]
systemslab = "http://systemslab"
host_tags = ["z2.baremetal"]
shape = "z2.g"
image = "spool/images/debian-13-gpu@golden"
judge = true

[[rack.reviewers]]
identity = "barry"
model = "Qwen/Qwen3.5-9B@latest/gguf/q4_k_m@latest"
model_name = "qwen3.5-9b"

[[rack.reviewers]]
identity = "other_barry"
model = "Qwen/Qwen3.5-9B@latest/gguf/q4_k_m@latest"
model_name = "qwen3.5-9b"
"#;

    #[test]
    fn a_rack_that_judges_needs_no_llm_profiles_at_all() {
        // The point of the rack judge: no endpoint, no key, no diff leaving the
        // rack, and nothing in secrets.env but the webhook secret.
        let f = write_tmp(RACK_ONLY);
        Config::load(f.path()).expect("a rack-only config must load");
    }

    #[test]
    fn a_rack_that_does_not_judge_still_needs_one() {
        let f = write_tmp(&RACK_ONLY.replace("judge = true", "judge = false"));
        let err = Config::load(f.path()).unwrap_err().to_string();
        assert!(err.contains("[llm.judge]"), "unexpected error: {err}");
    }

    #[test]
    fn a_judge_on_concurrent_reviewers_is_refused_at_startup() {
        // Caught when the config is read rather than when a pull request
        // arrives: the failure is in the file, not in the pull request.
        let f = write_tmp(&RACK_ONLY.replace(
            "image = \"spool/images/debian-13-gpu@golden\"",
            "image = \"spool/images/debian-13-gpu@golden\"\nplacement = \"concurrent\"",
        ));
        let err = Config::load(f.path()).unwrap_err().to_string();
        assert!(err.contains("sequential"), "unexpected error: {err}");
    }

    #[test]
    fn without_a_rack_every_profile_is_still_required() {
        let f = write_tmp(&RACK_ONLY[..RACK_ONLY.find("[rack]").unwrap()]);
        let err = Config::load(f.path()).unwrap_err().to_string();
        assert!(err.contains("[llm.barry]"), "unexpected error: {err}");
    }

    #[test]
    fn loads_minimal_valid_config() {
        let f = write_tmp(
            r#"
            [server]
            listen = "0.0.0.0:8080"

            [github.barry]
            app_id = 1
            private_key_path = "/tmp/b.pem"
            webhook_secret_env = "WS"
            [github.other_barry]
            app_id = 2
            private_key_path = "/tmp/ob.pem"
            [github.other_other_barry]
            app_id = 3
            private_key_path = "/tmp/oob.pem"

            [storage]
            sqlite_path = "/tmp/b.db"

            [dispatcher]

            [llm.barry]
            provider = "anthropic"
            endpoint = "https://api.anthropic.com"
            model = "x"
            [llm.other_barry]
            provider = "openai"
            endpoint = "http://localhost:1/v1"
            model = "x"
            [llm.other_other_barry]
            provider = "openai"
            endpoint = "https://api.openai.com/v1"
            model = "x"
            [llm.judge]
            provider = "anthropic"
            endpoint = "https://api.anthropic.com"
            model = "x"

            [confer]
            allowed = ["author", "write", "admin"]
        "#,
        );
        let cfg = Config::load(f.path()).expect("should load");
        assert_eq!(cfg.dispatcher.worker_count, 4);
    }

    /// The minimal valid config with `extra` prepended at the top level.
    fn minimal_with(extra: &str) -> tempfile::NamedTempFile {
        write_tmp(&format!(
            r#"
            {extra}

            [server]
            listen = "0.0.0.0:8080"
            [github.barry]
            app_id = 1
            private_key_path = "/tmp/b.pem"
            webhook_secret_env = "WS"
            [github.other_barry]
            app_id = 2
            private_key_path = "/tmp/ob.pem"
            [github.other_other_barry]
            app_id = 3
            private_key_path = "/tmp/oob.pem"
            [storage]
            sqlite_path = "/tmp/b.db"
            [dispatcher]
            [llm.barry]
            provider = "anthropic"
            endpoint = "https://api.anthropic.com"
            model = "x"
            [llm.other_barry]
            provider = "openai"
            endpoint = "http://localhost:1/v1"
            model = "x"
            [llm.other_other_barry]
            provider = "openai"
            endpoint = "https://api.openai.com/v1"
            model = "x"
            [llm.judge]
            provider = "anthropic"
            endpoint = "https://api.anthropic.com"
            model = "x"
        "#
        ))
    }

    #[test]
    fn commanders_load() {
        let f = minimal_with(r#"commands_from = ["brayniac"]"#);
        let cfg = Config::load(f.path()).expect("should load");
        assert_eq!(cfg.commands_from, Some(vec!["brayniac".to_string()]));
    }

    #[test]
    fn a_commander_written_as_a_mention_is_refused() {
        // `@brayniac` is how it is typed in a comment and never how GitHub
        // delivers it, so it would silently match nobody.
        let f = minimal_with(r#"commands_from = ["@brayniac"]"#);
        let err = Config::load(f.path()).unwrap_err().to_string();
        assert!(err.contains("not a bare GitHub login"), "{err}");
    }

    #[test]
    fn rejects_missing_default_llm_profile() {
        let f = write_tmp(
            r#"
            [server]
            listen = "0.0.0.0:8080"
            [github.barry]
            app_id = 1
            private_key_path = "/tmp/b.pem"
            webhook_secret_env = "WS"
            [github.other_barry]
            app_id = 2
            private_key_path = "/tmp/ob.pem"
            [github.other_other_barry]
            app_id = 3
            private_key_path = "/tmp/oob.pem"
            [storage]
            sqlite_path = "/tmp/b.db"
            [dispatcher]
            [llm.other]
            provider = "openai"
            endpoint = "http://localhost:1234/v1"
            model = "local"
        "#,
        );
        // Missing [llm.barry] → validation error
        let err = Config::load(f.path()).unwrap_err();
        assert!(matches!(err, ConfigError::Validate(_)));
    }

    #[test]
    fn rejects_zero_workers() {
        let f = write_tmp(
            r#"
            [server]
            listen = "0.0.0.0:8080"
            [github.barry]
            app_id = 1
            private_key_path = "/tmp/b.pem"
            webhook_secret_env = "WS"
            [github.other_barry]
            app_id = 2
            private_key_path = "/tmp/ob.pem"
            [github.other_other_barry]
            app_id = 3
            private_key_path = "/tmp/oob.pem"
            [storage]
            sqlite_path = "/tmp/b.db"
            [dispatcher]
            worker_count = 0
            [llm.barry]
            provider = "anthropic"
            endpoint = "https://api.anthropic.com"
            model = "x"
            [llm.other_barry]
            provider = "openai"
            endpoint = "http://localhost:1/v1"
            model = "x"
            [llm.other_other_barry]
            provider = "openai"
            endpoint = "https://api.openai.com/v1"
            model = "x"
            [llm.judge]
            provider = "anthropic"
            endpoint = "https://api.anthropic.com"
            model = "x"
        "#,
        );
        assert!(Config::load(f.path()).is_err());
    }

    #[test]
    fn loads_three_identity_config() {
        let f = write_tmp(
            r#"
            [server]
            listen = "0.0.0.0:8080"

            [github.barry]
            app_id = 1
            private_key_path = "/tmp/b.pem"
            webhook_secret_env = "WS"

            [github.other_barry]
            app_id = 2
            private_key_path = "/tmp/ob.pem"

            [github.other_other_barry]
            app_id = 3
            private_key_path = "/tmp/oob.pem"

            [storage]
            sqlite_path = "/tmp/b.db"

            [dispatcher]

            [llm.barry]
            provider = "anthropic"
            endpoint = "https://api.anthropic.com"
            model = "claude-opus-4-7"

            [llm.other_barry]
            provider = "openai"
            endpoint = "http://localhost:11434/v1"
            model = "qwen"

            [llm.other_other_barry]
            provider = "openai"
            endpoint = "https://api.openai.com/v1"
            model = "gpt-5"

            [llm.judge]
            provider = "anthropic"
            endpoint = "https://api.anthropic.com"
            model = "claude-haiku-4-5-20251001"

            [confer]
            allowed = ["author", "write", "admin"]
            max_per_pr = 2
        "#,
        );
        let cfg = Config::load(f.path()).expect("should load");
        assert_eq!(cfg.github.barry.app_id, 1);
        assert_eq!(cfg.github.other_barry.app_id, 2);
        assert_eq!(cfg.github.other_other_barry.app_id, 3);
        assert_eq!(cfg.confer.max_per_pr, 2);
        assert!(cfg.confer.allowed.iter().any(|r| r == "write"));
        assert!(cfg.llm.contains_key("judge"));
    }

    #[test]
    fn rejects_missing_other_barry_when_multi_review_used() {
        // Compatibility: the legacy single-Barry shape is NOT supported.
        // All three [github.*] blocks are required.
        let f = write_tmp(
            r#"
            [server]
            listen = "0.0.0.0:8080"

            [github.barry]
            app_id = 1
            private_key_path = "/tmp/b.pem"
            webhook_secret_env = "WS"

            [storage]
            sqlite_path = "/tmp/b.db"
            [dispatcher]
            [llm.barry]
            provider = "anthropic"
            endpoint = "https://api.anthropic.com"
            model = "x"
        "#,
        );
        // Missing other_barry / other_other_barry → parse error from required field.
        let err = Config::load(f.path()).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Parse { .. } | ConfigError::Validate(_)
        ));
    }
}
