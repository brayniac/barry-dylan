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

#[derive(Debug, Clone, Default)]
pub struct PersonaOverrides {
    pub security: Option<PathBuf>,
    pub correctness: Option<PathBuf>,
    pub style: Option<PathBuf>,
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
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].name, "security");
        assert!(!p[0].prompt.is_empty());
        assert!(!p[1].prompt.is_empty());
        assert!(!p[2].prompt.is_empty());
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
}
