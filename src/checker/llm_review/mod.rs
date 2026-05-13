//! LLM-driven PR review checker.

pub mod prompt;
pub mod parse;

use crate::checker::{Checker, CheckerCtx, CheckerOutcome, OutcomeStatus};
use crate::config::repo::RepoConfig;
use crate::github::pr::{ChangedFile, ReviewCommentInput};
use crate::llm::{LlmClient, LlmMessage, LlmRequest, Role};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::Arc;

pub const REVIEW_MARKER: &str = "<!-- barry-bot:llm-review:v1 -->";

pub struct LlmReviewChecker {
    pub client: Arc<dyn LlmClient>,
    pub max_tokens: u32,
}

#[async_trait]
impl Checker for LlmReviewChecker {
    fn name(&self) -> &'static str { "barry/llm-review" }
    fn enabled(&self, cfg: &RepoConfig) -> bool { cfg.llm_review.enabled }

    async fn run(&self, ctx: &CheckerCtx) -> anyhow::Result<CheckerOutcome> {
        let rule = &ctx.repo_cfg.llm_review;
        // Minimize prior bot reviews bearing our marker.
        for r in &ctx.prior_bot_reviews {
            if r.body.contains(REVIEW_MARKER) {
                let _ = ctx.gh.minimize_comment(&r.node_id).await;
            }
        }

        let chunks = prompt::chunk_files(&ctx.files, rule.max_diff_tokens, &rule.exclude_paths);
        if chunks.is_empty() {
            return Ok(CheckerOutcome::success(self.name(), "no reviewable files"));
        }

        let system = prompt::system_prompt(&rule.focus);
        let mut per_file_summaries: Vec<String> = Vec::new();
        let mut findings: Vec<parse::ParsedFinding> = Vec::new();
        for chunk in &chunks {
            let user = prompt::user_prompt(chunk, prompt::RESPONSE_FORMAT);
            let req = LlmRequest {
                system: Some(system.clone()),
                messages: vec![LlmMessage { role: Role::User, content: user }],
                max_tokens: self.max_tokens, temperature: 0.0,
            };
            let r = match self.client.complete(&req).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(?e, "llm call failed");
                    return Ok(CheckerOutcome::neutral(self.name(),
                        format!("llm call failed: {e}")));
                }
            };
            match parse::parse(&r.text) {
                Ok(p) => {
                    findings.extend(p.findings);
                    if !p.summary.is_empty() { per_file_summaries.push(p.summary); }
                }
                Err(e) => {
                    tracing::warn!(?e, body = %r.text, "llm output parse failed");
                    return Ok(CheckerOutcome::neutral(self.name(), "llm output unparseable"));
                }
            }
        }

        let inline_comments = to_inline_comments(&ctx.files, &findings);
        let body = format!(
            "{REVIEW_MARKER}\n**barry-bot LLM review** ({} chunk{}, {} finding{})\n\n{}",
            chunks.len(), plural(chunks.len()),
            findings.len(), plural(findings.len()),
            per_file_summaries.join("\n\n"),
        );

        Ok(CheckerOutcome {
            checker_name: self.name(),
            status: if findings.is_empty() { OutcomeStatus::Success } else { OutcomeStatus::Neutral },
            summary: body,
            text: None,
            inline_comments,
            issue_comment: None,
            add_labels: vec![],
        })
    }
}

fn plural(n: usize) -> &'static str { if n == 1 { "" } else { "s" } }

/// Convert (file, line) findings to PR-review inline comments by computing
/// the GitHub "position" (line index within the file's unified patch).
fn to_inline_comments(files: &[ChangedFile], findings: &[parse::ParsedFinding]) -> Vec<ReviewCommentInput> {
    let mut by_file: BTreeMap<&str, &ChangedFile> = BTreeMap::new();
    for f in files { by_file.insert(f.filename.as_str(), f); }
    findings.iter().filter_map(|f| {
        let cf = by_file.get(f.file.as_str())?;
        let patch = cf.patch.as_deref()?;
        let pos = patch_position_for_new_line(patch, f.line)?;
        Some(ReviewCommentInput {
            path: f.file.clone(),
            position: pos as i64,
            body: f.message.clone(),
        })
    }).collect()
}

/// Walk a unified diff hunk and return the 1-based "position" of the line in
/// the patch that corresponds to the given new-side line number, or None if
/// the line is not in any hunk (e.g. unchanged context far from edits).
fn patch_position_for_new_line(patch: &str, target_new_line: u32) -> Option<u32> {
    let mut pos = 0u32;
    let mut new_line = 0u32;
    for line in patch.lines() {
        if line.starts_with("@@") {
            // parse new-side start: @@ -a,b +c,d @@
            if let Some(plus) = line.split_whitespace().find(|s| s.starts_with('+')) {
                let n = plus.trim_start_matches('+');
                let start: u32 = n.split(',').next()?.parse().ok()?;
                new_line = start.saturating_sub(1);
            }
            pos += 1; // hunk header counts as a position
            continue;
        }
        pos += 1;
        if line.starts_with('-') { continue; }
        new_line += 1;
        if new_line == target_new_line { return Some(pos); }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_for_added_line() {
        let patch = "@@ -1,2 +1,3 @@\n a\n+b\n c\n";
        // new lines: 1=a, 2=b, 3=c → positions: 1=hdr, 2=a, 3=b, 4=c
        assert_eq!(patch_position_for_new_line(patch, 2), Some(3));
        assert_eq!(patch_position_for_new_line(patch, 3), Some(4));
    }

    #[test]
    fn position_returns_none_for_unseen_line() {
        let patch = "@@ -1,1 +1,1 @@\n a\n";
        assert_eq!(patch_position_for_new_line(patch, 99), None);
    }
}
