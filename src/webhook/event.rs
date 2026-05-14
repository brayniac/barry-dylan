use serde::Deserialize;

/// Minimal-subset deserialization of the webhook events we care about.
/// We intentionally do not model the full payload; we extract only what we need.
#[derive(Debug, Deserialize)]
#[serde(tag = "_kind")]
pub enum Event {} // unused; we dispatch by header.

#[derive(Debug, Deserialize)]
pub struct PullRequestEvent {
    pub action: String,
    pub number: i64,
    pub installation: Installation,
    pub repository: Repository,
    pub pull_request: PullRequestRef,
}

#[derive(Debug, Deserialize)]
pub struct IssueCommentEvent {
    pub action: String,
    pub installation: Installation,
    pub repository: Repository,
    pub issue: Issue,
    pub comment: Comment,
    pub sender: User,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Installation {
    pub id: i64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Repository {
    pub name: String,
    pub owner: User,
    pub default_branch: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct User {
    pub login: String,
}

#[derive(Debug, Deserialize)]
pub struct PullRequestRef {
    pub number: i64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub user: User,
    pub draft: bool,
    pub state: String,
    pub head: GitRef,
    pub base: GitRef,
}

#[derive(Debug, Deserialize)]
pub struct GitRef {
    pub sha: String,
    pub r#ref: String,
}

#[derive(Debug, Deserialize)]
pub struct Issue {
    pub number: i64,
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct Comment {
    pub id: i64,
    pub body: String,
    pub user: User,
    pub node_id: String,
}

#[derive(Debug)]
pub enum InboundEvent {
    PullRequest(PullRequestEvent),
    IssueComment(IssueCommentEvent),
    /// Anything else we receive but ignore.
    Ignored(&'static str),
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("missing X-GitHub-Event header")]
    MissingEventHeader,
    #[error("malformed JSON: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn parse(event_header: Option<&str>, body: &[u8]) -> Result<InboundEvent, ParseError> {
    let evt = event_header.ok_or(ParseError::MissingEventHeader)?;
    match evt {
        "pull_request" => Ok(InboundEvent::PullRequest(serde_json::from_slice(body)?)),
        "issue_comment" => Ok(InboundEvent::IssueComment(serde_json::from_slice(body)?)),
        "ping" => Ok(InboundEvent::Ignored("ping")),
        other => {
            let _ = other;
            Ok(InboundEvent::Ignored("other"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pull_request_event() {
        let payload = serde_json::json!({
            "action": "opened",
            "number": 7,
            "installation": { "id": 99 },
            "repository": { "name": "r", "owner": { "login": "o" }, "default_branch": "main" },
            "pull_request": {
                "number": 7,
                "title": "feat: add x",
                "body": "Long enough body for sanity.",
                "user": { "login": "alice" },
                "draft": false,
                "state": "open",
                "head": { "sha": "abc", "ref": "feat-x" },
                "base": { "sha": "def", "ref": "main" }
            }
        });
        let body = serde_json::to_vec(&payload).unwrap();
        match parse(Some("pull_request"), &body).unwrap() {
            InboundEvent::PullRequest(e) => {
                assert_eq!(e.action, "opened");
                assert_eq!(e.pull_request.title, "feat: add x");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn ignores_ping() {
        let body = b"{}";
        match parse(Some("ping"), body).unwrap() {
            InboundEvent::Ignored(_) => {}
            _ => panic!(),
        }
    }
}
