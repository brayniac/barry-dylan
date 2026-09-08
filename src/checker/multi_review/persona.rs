use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Persona {
    pub name: &'static str,
    pub prompt: Arc<String>,
}

const SECURITY: &str = include_str!("prompts/security.md");
const CORRECTNESS: &str = include_str!("prompts/correctness.md");
const STYLE: &str = include_str!("prompts/style.md");
const RUST: &str = include_str!("prompts/rust.md");

#[derive(Debug, Clone, Default)]
pub struct PersonaOverrides {
    pub security: Option<PathBuf>,
    pub correctness: Option<PathBuf>,
    pub style: Option<PathBuf>,
    pub rust: Option<PathBuf>,
}

/// Map the TOML persona-override config into the resolver's input type.
pub fn overrides_from_config(p: &crate::config::PersonaOverridesConfig) -> PersonaOverrides {
    PersonaOverrides {
        security: p.security.as_ref().and_then(|x| x.prompt_path.clone()),
        correctness: p.correctness.as_ref().and_then(|x| x.prompt_path.clone()),
        style: p.style.as_ref().and_then(|x| x.prompt_path.clone()),
        rust: p.rust.as_ref().and_then(|x| x.prompt_path.clone()),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PersonaError {
    #[error("reading persona prompt {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub fn resolve(overrides: &PersonaOverrides) -> Result<Vec<Persona>, PersonaError> {
    Ok(vec![
        Persona {
            name: "security",
            prompt: Arc::new(load(overrides.security.as_deref(), SECURITY)?),
        },
        Persona {
            name: "correctness",
            prompt: Arc::new(load(overrides.correctness.as_deref(), CORRECTNESS)?),
        },
        Persona {
            name: "style",
            prompt: Arc::new(load(overrides.style.as_deref(), STYLE)?),
        },
        Persona {
            name: "rust",
            prompt: Arc::new(load(overrides.rust.as_deref(), RUST)?),
        },
    ])
}

fn load(path: Option<&std::path::Path>, default: &str) -> Result<String, PersonaError> {
    match path {
        None => Ok(default.to_string()),
        Some(p) => std::fs::read_to_string(p).map_err(|e| PersonaError::Io {
            path: p.into(),
            source: e,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn defaults_all_loaded() {
        let p = resolve(&PersonaOverrides::default()).unwrap();
        assert_eq!(p.len(), 4);
        assert_eq!(p[0].name, "security");
        assert!(!p[0].prompt.is_empty());
        assert!(!p[1].prompt.is_empty());
        assert!(!p[2].prompt.is_empty());
        assert!(!p[3].prompt.is_empty());
    }

    #[test]
    fn override_replaces_default() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "custom security prompt").unwrap();
        let ovr = PersonaOverrides {
            security: Some(f.path().to_path_buf()),
            ..Default::default()
        };
        let p = resolve(&ovr).unwrap();
        assert!(p[0].prompt.contains("custom security prompt"));
        // Other personas still load defaults.
        assert!(!p[1].prompt.contains("custom"));
    }

    #[test]
    fn missing_override_file_errors() {
        let ovr = PersonaOverrides {
            style: Some(PathBuf::from("/nonexistent/path/to/prompt.md")),
            ..Default::default()
        };
        let err = resolve(&ovr).unwrap_err();
        assert!(matches!(err, PersonaError::Io { .. }));
    }

    #[test]
    fn rust_persona_prompt_exists() {
        let p = resolve(&PersonaOverrides::default()).unwrap();
        let rust_persona = p.iter().find(|persona| persona.name == "rust").unwrap();
        assert!(!rust_persona.prompt.is_empty());
        // Verify it mentions key Rust concepts
        let prompt = rust_persona.prompt.to_lowercase();
        assert!(prompt.contains("ownership"));
        assert!(prompt.contains("clone"));
    }

    #[test]
    fn overrides_from_config_maps_prompt_paths() {
        let cfg = crate::config::PersonaOverridesConfig {
            security: Some(crate::config::PersonaOverride {
                prompt_path: Some(PathBuf::from("/tmp/sec.md")),
            }),
            correctness: None,
            style: None,
            rust: None,
        };

        let o = overrides_from_config(&cfg);

        assert_eq!(o.security, Some(PathBuf::from("/tmp/sec.md")));
        assert_eq!(o.correctness, None);
        assert_eq!(o.style, None);
        assert_eq!(o.rust, None);
    }

    #[test]
    fn resolve_returns_four_personas_with_defaults() {
        let personas = resolve(&PersonaOverrides::default()).unwrap();
        let names: Vec<&str> = personas.iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["security", "correctness", "style", "rust"]);
        assert!(personas.iter().all(|p| !p.prompt.is_empty()));
    }
}
