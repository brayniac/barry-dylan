use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ParsedReview {
    pub findings: Vec<ParsedFinding>,
    #[serde(default)]
    pub summary: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ParsedFinding {
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

/// Locate the first balanced JSON object in `text` and parse it.
/// Tolerates model output that wraps JSON in prose or markdown fences.
pub fn parse(text: &str) -> Result<ParsedReview, ParseError> {
    let bytes = text.as_bytes();
    let mut start = None;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            if esc { esc = false; continue; }
            if b == b'\\' { esc = true; continue; }
            if b == b'"' { in_str = false; }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => { if depth == 0 { start = Some(i); } depth += 1; }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start {
                        let slice = &text[s..=i];
                        return Ok(serde_json::from_str(slice)?);
                    }
                }
            }
            _ => {}
        }
    }
    Err(ParseError::NoJson)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clean_json() {
        let r = parse(r#"{"findings":[{"file":"a.rs","line":3,"message":"hi"}],"summary":"x"}"#).unwrap();
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.summary, "x");
    }

    #[test]
    fn parses_json_in_fenced_block() {
        let r = parse("Some preamble\n```json\n{\"findings\":[],\"summary\":\"ok\"}\n```\n").unwrap();
        assert_eq!(r.summary, "ok");
    }

    #[test]
    fn rejects_no_json() {
        let err = parse("totally not json").unwrap_err();
        assert!(matches!(err, ParseError::NoJson));
    }

    #[test]
    fn handles_quotes_inside_strings() {
        let r = parse(r#"{"findings":[{"file":"a","line":1,"message":"he said \"hi\""}],"summary":""}"#).unwrap();
        assert_eq!(r.findings[0].message, r#"he said "hi""#);
    }
}
