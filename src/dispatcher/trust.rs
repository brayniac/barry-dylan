use crate::github::pr::BotComment;

pub const APPROVE_MARKER: &str = "<!-- barry-dylan:approved:v1 -->";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust { Trusted, NeedsApproval }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarryCommand { Approve, Review, Unknown, NotACommand }

/// Author is "trusted" iff their permission level is one of admin/maintain/write
/// OR a prior bot comment contains the sticky approve marker.
pub fn evaluate_trust(permission: &str, prior_bot_comments: &[BotComment]) -> Trust {
    if matches!(permission, "admin" | "maintain" | "write") {
        return Trust::Trusted;
    }
    if prior_bot_comments.iter().any(|c| c.body.contains(APPROVE_MARKER)) {
        return Trust::Trusted;
    }
    Trust::NeedsApproval
}

pub fn parse_command(body: &str) -> BarryCommand {
    let mut parts = body.split_whitespace();
    match parts.next() {
        Some("/barry") => match parts.next() {
            Some("approve") => BarryCommand::Approve,
            Some("review") => BarryCommand::Review,
            Some(_) => BarryCommand::Unknown,
            None => BarryCommand::Unknown,
        }
        _ => BarryCommand::NotACommand,
    }
}

pub fn approve_comment_body() -> String {
    format!("{APPROVE_MARKER}\nReview enabled for this PR by a maintainer.")
}

pub const NEEDS_APPROVAL_MARKER: &str = "<!-- barry-dylan:needs-approval:v1 -->";

pub fn needs_approval_body(author: &str) -> String {
    format!(
        "{NEEDS_APPROVAL_MARKER}\nHi @{author} — automated review is gated for PRs from contributors without write access. A maintainer can comment `/barry approve` to enable barry-dylan on this PR."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmt(body: &str) -> BotComment {
        BotComment { id: 1, node_id: "n".into(), body: body.into(), author: "barry-dylan[bot]".into() }
    }

    #[test]
    fn write_perm_trusted() {
        assert_eq!(evaluate_trust("write", &[]), Trust::Trusted);
    }
    #[test]
    fn read_perm_untrusted_without_marker() {
        assert_eq!(evaluate_trust("read", &[]), Trust::NeedsApproval);
    }
    #[test]
    fn read_perm_trusted_via_sticky_marker() {
        assert_eq!(evaluate_trust("read", &[cmt(APPROVE_MARKER)]), Trust::Trusted);
    }

    #[test]
    fn parse_commands() {
        assert_eq!(parse_command("/barry approve"), BarryCommand::Approve);
        assert_eq!(parse_command("/barry review please"), BarryCommand::Review);
        assert_eq!(parse_command("/barry whoknows"), BarryCommand::Unknown);
        assert_eq!(parse_command("just a comment"), BarryCommand::NotACommand);
    }
}
