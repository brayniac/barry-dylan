use crate::config::{LlmProfile, PersonaOverridesConfig};
use serde::Deserialize;
use std::path::Path;

/// Configuration for a single offline review.
///
/// Deliberately not [`crate::config::Config`]: producing a review needs one LLM
/// endpoint and optional persona prompts. It does not need GitHub app
/// credentials, a SQLite path, or a listen address, and a job spec should not
/// have to carry them.
#[derive(Debug, Clone, Deserialize)]
pub struct OfflineConfig {
    /// The single model that runs every persona and the synthesis pass.
    pub llm: LlmProfile,
    #[serde(default)]
    pub personas: PersonaOverridesConfig,
}

pub fn load(path: &Path) -> anyhow::Result<OfflineConfig> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading offline config {}: {e}", path.display()))?;
    toml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parsing offline config {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_minimal_config() {
        let toml_text = r#"
[llm]
provider = "openai"
endpoint = "http://127.0.0.1:8080/v1"
model = "qwen2.5-coder-32b"
"#;
        let cfg: OfflineConfig = toml::from_str(toml_text).unwrap();
        assert_eq!(cfg.llm.endpoint, "http://127.0.0.1:8080/v1");
        assert_eq!(cfg.llm.model, "qwen2.5-coder-32b");
        assert_eq!(cfg.llm.max_tokens, 8192); // serde default
        assert!(cfg.personas.security.is_none());
    }

    #[test]
    fn rejects_a_config_with_no_llm_section() {
        let err = toml::from_str::<OfflineConfig>("").unwrap_err();
        assert!(err.to_string().contains("llm"), "unexpected error: {err}");
    }
}
