use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Identity {
    Barry,
    OtherBarry,
    OtherOtherBarry,
}

impl Identity {
    pub fn label(self) -> &'static str {
        match self {
            Identity::Barry => "Barry",
            Identity::OtherBarry => "Other Barry",
            Identity::OtherOtherBarry => "Other Other Barry",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Identity::Barry => "barry",
            Identity::OtherBarry => "other_barry",
            Identity::OtherOtherBarry => "other_other_barry",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_distinct() {
        assert_ne!(Identity::Barry.label(), Identity::OtherBarry.label());
        assert_ne!(
            Identity::OtherBarry.label(),
            Identity::OtherOtherBarry.label()
        );
    }

    #[test]
    fn slugs_match_config_keys() {
        assert_eq!(Identity::Barry.slug(), "barry");
        assert_eq!(Identity::OtherBarry.slug(), "other_barry");
        assert_eq!(Identity::OtherOtherBarry.slug(), "other_other_barry");
    }
}
