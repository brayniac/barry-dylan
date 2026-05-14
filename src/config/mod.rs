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
    pub app_id: u64,
    pub private_key_path: PathBuf,
    pub webhook_secret_env: String,
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
        if self.llm.is_empty() {
            return Err(ConfigError::Validate(
                "at least one [llm.<name>] profile required".into(),
            ));
        }
        if !self.llm.contains_key("default") {
            return Err(ConfigError::Validate(
                "an [llm.default] profile is required".into(),
            ));
        }
        if self.dispatcher.worker_count == 0 {
            return Err(ConfigError::Validate(
                "dispatcher.worker_count must be > 0".into(),
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

            [github]
            app_id = 1
            private_key_path = "/tmp/k.pem"
            webhook_secret_env = "WS"

            [storage]
            sqlite_path = "/tmp/b.db"

            [dispatcher]

            [llm.default]
            provider = "anthropic"
            endpoint = "https://api.anthropic.com"
            model = "claude-sonnet-4-6"
        "#,
        );
        let cfg = Config::load(f.path()).expect("should load");
        assert_eq!(cfg.dispatcher.worker_count, 4);
        assert_eq!(cfg.llm["default"].provider, LlmProviderKind::Anthropic);
    }

    #[test]
    fn rejects_missing_default_llm_profile() {
        let f = write_tmp(
            r#"
            [server]
            listen = "0.0.0.0:8080"
            [github]
            app_id = 1
            private_key_path = "/tmp/k.pem"
            webhook_secret_env = "WS"
            [storage]
            sqlite_path = "/tmp/b.db"
            [dispatcher]
            [llm.other]
            provider = "openai"
            endpoint = "http://localhost:1234/v1"
            model = "local"
        "#,
        );
        let err = Config::load(f.path()).unwrap_err();
        assert!(matches!(err, ConfigError::Validate(_)));
    }

    #[test]
    fn rejects_zero_workers() {
        let f = write_tmp(
            r#"
            [server]
            listen = "0.0.0.0:8080"
            [github]
            app_id = 1
            private_key_path = "/tmp/k.pem"
            webhook_secret_env = "WS"
            [storage]
            sqlite_path = "/tmp/b.db"
            [dispatcher]
            worker_count = 0
            [llm.default]
            provider = "anthropic"
            endpoint = "https://api.anthropic.com"
            model = "x"
        "#,
        );
        assert!(Config::load(f.path()).is_err());
    }
}
