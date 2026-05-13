use serde::Deserialize;

#[derive(Debug, Deserialize, Clone, Default)]
pub struct RepoConfig {
    #[serde(default)]
    pub hygiene: Hygiene,
    #[serde(default)]
    pub llm_review: LlmReview,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Hygiene {
    #[serde(default)]
    pub title: TitleRule,
    #[serde(default)]
    pub description: DescriptionRule,
    #[serde(default)]
    pub size: SizeRule,
    #[serde(default)]
    pub autolabel: Autolabel,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TitleRule {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_title_pattern")]
    pub pattern: String,
}

impl Default for TitleRule {
    fn default() -> Self { Self { enabled: true, pattern: default_title_pattern() } }
}

fn default_title_pattern() -> String {
    // Conventional Commits types per https://www.conventionalcommits.org/.
    // Optional scope in parens, optional `!` breaking-change marker, colon-space, then text.
    r"^(feat|fix|chore|docs|refactor|test|perf|ci|build|style|revert)(\([^)]+\))?!?: .+".into()
}

#[derive(Debug, Deserialize, Clone)]
pub struct DescriptionRule {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_min_len")]
    pub min_length: usize,
    #[serde(default)]
    pub require_template_sections: Vec<String>,
}

impl Default for DescriptionRule {
    fn default() -> Self {
        Self { enabled: true, min_length: default_min_len(), require_template_sections: vec![] }
    }
}

fn default_min_len() -> usize { 20 }

#[derive(Debug, Deserialize, Clone)]
pub struct SizeRule {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_warn_lines")]
    pub warn_lines: u32,
    #[serde(default = "default_warn_files")]
    pub warn_files: u32,
}

impl Default for SizeRule {
    fn default() -> Self {
        Self { enabled: true, warn_lines: default_warn_lines(), warn_files: default_warn_files() }
    }
}

fn default_warn_lines() -> u32 { 500 }
fn default_warn_files() -> u32 { 20 }

#[derive(Debug, Deserialize, Clone)]
pub struct Autolabel {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub rules: Vec<AutolabelRule>,
}

impl Default for Autolabel {
    fn default() -> Self { Self { enabled: true, rules: vec![] } }
}

#[derive(Debug, Deserialize, Clone)]
pub struct AutolabelRule {
    pub paths: Vec<String>,
    pub labels: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LlmReview {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_profile")]
    pub provider_profile: String,
    #[serde(default)]
    pub focus: String,
    #[serde(default)]
    pub exclude_paths: Vec<String>,
    #[serde(default = "default_max_diff_tokens")]
    pub max_diff_tokens: u32,
}

impl Default for LlmReview {
    fn default() -> Self {
        Self {
            enabled: true,
            provider_profile: default_profile(),
            focus: String::new(),
            exclude_paths: vec![],
            max_diff_tokens: default_max_diff_tokens(),
        }
    }
}

fn default_profile() -> String { "default".into() }
fn default_max_diff_tokens() -> u32 { 100_000 }
fn yes() -> bool { true }

impl RepoConfig {
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_parses_to_defaults() {
        let r = RepoConfig::parse("").unwrap();
        assert!(r.hygiene.title.enabled);
        assert!(r.llm_review.enabled);
        assert_eq!(r.llm_review.provider_profile, "default");
    }

    #[test]
    fn disables_what_is_set_false() {
        let r = RepoConfig::parse(r#"
            [hygiene.title]
            enabled = false
        "#).unwrap();
        assert!(!r.hygiene.title.enabled);
        assert!(r.hygiene.description.enabled);
    }

    #[test]
    fn autolabel_rules_parse() {
        let r = RepoConfig::parse(r#"
            [hygiene.autolabel]
            rules = [
                { paths = ["src/x/**"], labels = ["area/x"] },
            ]
        "#).unwrap();
        assert_eq!(r.hygiene.autolabel.rules.len(), 1);
        assert_eq!(r.hygiene.autolabel.rules[0].labels[0], "area/x");
    }
}
