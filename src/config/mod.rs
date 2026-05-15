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
}

fn default_max_tokens() -> u32 {
    8192
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
        for required in ["barry", "other_barry", "other_other_barry", "judge"] {
            if !self.llm.contains_key(required) {
                return Err(ConfigError::Validate(format!(
                    "an [llm.{required}] profile is required"
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
