use super::parse_util::locate_json;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Approve,
    Comment,
    RequestChanges,
}

impl Outcome {
    pub fn check_conclusion(self) -> crate::github::check_run::CheckConclusion {
        use crate::github::check_run::CheckConclusion::*;
        match self {
            Outcome::Approve => Success,
            Outcome::Comment => Neutral,
            Outcome::RequestChanges => Failure,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct UnifiedReview {
    /// One of "approve", "comment", "request_changes".
    pub outcome: Outcome,
    /// Short prose summary visible at the top of the posted review.
    pub summary: String,
    /// Inline findings keyed to (file, line). Optional.
    #[serde(default)]
    pub findings: Vec<UnifiedFinding>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UnifiedFinding {
    pub file: String,
    pub line: u32,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("could not locate JSON object in model output")]
    NoJson,
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Locate the first balanced JSON object in `text` and parse a UnifiedReview.
/// Tolerates model output that wraps JSON in prose or markdown fences.
pub fn parse(text: &str) -> Result<UnifiedReview, ParseError> {
    let slice = locate_json(text).ok_or(ParseError::NoJson)?;
    Ok(serde_json::from_str(slice)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_approve() {
        let r = parse(r#"{"outcome":"approve","summary":"LGTM","findings":[]}"#).unwrap();
        assert_eq!(r.outcome, Outcome::Approve);
        assert_eq!(r.summary, "LGTM");
        assert!(r.findings.is_empty());
    }

    #[test]
    fn parses_request_changes_with_findings() {
        let r = parse(
            r#"{"outcome":"request_changes","summary":"missing checks",
                "findings":[{"file":"a.rs","line":3,"message":"unchecked unwrap"}]}"#,
        )
        .unwrap();
        assert_eq!(r.outcome, Outcome::RequestChanges);
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].line, 3);
    }

    #[test]
    fn parses_in_fenced_block() {
        let r =
            parse("preamble\n```json\n{\"outcome\":\"comment\",\"summary\":\"x\"}\n```").unwrap();
        assert_eq!(r.outcome, Outcome::Comment);
    }

    #[test]
    fn rejects_no_json() {
        assert!(matches!(parse("not json"), Err(ParseError::NoJson)));
    }

    #[test]
    fn outcome_maps_to_check_conclusion() {
        use crate::github::check_run::CheckConclusion;
        assert!(matches!(
            Outcome::Approve.check_conclusion(),
            CheckConclusion::Success
        ));
        assert!(matches!(
            Outcome::Comment.check_conclusion(),
            CheckConclusion::Neutral
        ));
        assert!(matches!(
            Outcome::RequestChanges.check_conclusion(),
            CheckConclusion::Failure
        ));
    }
}
