use crate::github::pr::ChangedFile;

/// Rough estimate of tokens for a string (chars/4). Cheap and good enough
/// for budgeting chunk boundaries.
pub fn est_tokens(s: &str) -> u32 { (s.chars().count() as u32) / 4 + 1 }

#[derive(Debug, Clone)]
pub struct FileChunk<'a> {
    pub files: Vec<&'a ChangedFile>,
    pub est_tokens: u32,
}

/// Group changed files into chunks each <= max_chunk_tokens (estimated).
/// Files larger than max_chunk_tokens land in their own chunk (the LLM call
/// will skip or summarize per the checker's logic).
pub fn chunk_files<'a>(files: &'a [ChangedFile], max_chunk_tokens: u32, excludes: &[String])
    -> Vec<FileChunk<'a>>
{
    let exc = build_globs(excludes);
    let included: Vec<&ChangedFile> = files.iter()
        .filter(|f| !exc.iter().any(|g| g.is_match(&f.filename)))
        .filter(|f| f.patch.is_some())
        .collect();
    let mut chunks: Vec<FileChunk> = Vec::new();
    let mut cur = FileChunk { files: vec![], est_tokens: 0 };
    for f in included {
        let t = est_tokens(f.patch.as_deref().unwrap_or(""));
        if t >= max_chunk_tokens {
            if !cur.files.is_empty() { chunks.push(std::mem::replace(&mut cur,
                FileChunk { files: vec![], est_tokens: 0 })); }
            chunks.push(FileChunk { files: vec![f], est_tokens: t });
            continue;
        }
        if cur.est_tokens + t > max_chunk_tokens && !cur.files.is_empty() {
            chunks.push(std::mem::replace(&mut cur,
                FileChunk { files: vec![], est_tokens: 0 }));
        }
        cur.files.push(f);
        cur.est_tokens += t;
    }
    if !cur.files.is_empty() { chunks.push(cur); }
    chunks
}

fn build_globs(patterns: &[String]) -> Vec<globset::GlobMatcher> {
    patterns.iter().filter_map(|p| globset::Glob::new(p).ok().map(|g| g.compile_matcher())).collect()
}

pub fn system_prompt(focus: &str) -> String {
    let base = "You are an experienced code reviewer. Review the diff and emit findings in the strict JSON format documented below. Only flag real issues with specific lines; do not restate the diff or praise code. If there are no real issues, return an empty `findings` array.";
    if focus.trim().is_empty() {
        base.to_string()
    } else {
        format!("{base}\n\nProject focus:\n{focus}")
    }
}

pub fn user_prompt(chunk: &FileChunk<'_>, format_spec: &str) -> String {
    let mut s = String::new();
    s.push_str("=== untrusted diff begins ===\n");
    for f in &chunk.files {
        s.push_str(&format!("File: {}\n", f.filename));
        s.push_str("```\n");
        if let Some(p) = &f.patch { s.push_str(p); }
        s.push_str("\n```\n");
    }
    s.push_str("=== untrusted diff ends ===\n\n");
    s.push_str(format_spec);
    s
}

pub const RESPONSE_FORMAT: &str = r#"
Output a single JSON object exactly matching:
{
  "findings": [
    {"file": "<path>", "line": <integer>, "message": "<one-paragraph issue description>"}
  ],
  "summary": "<one short overall summary>"
}
Do not include any text outside the JSON.
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::pr::ChangedFile;

    fn file(name: &str, patch_len: usize) -> ChangedFile {
        ChangedFile {
            filename: name.into(), status: "modified".into(),
            additions: 1, deletions: 0, changes: 1,
            patch: Some("x".repeat(patch_len)),
        }
    }

    #[test]
    fn small_files_packed_into_one_chunk() {
        let files = vec![file("a", 100), file("b", 100), file("c", 100)];
        let cs = chunk_files(&files, 1000, &[]);
        assert_eq!(cs.len(), 1);
    }

    #[test]
    fn exceeds_budget_splits() {
        let files = vec![file("a", 4000), file("b", 4000)]; // ~1000 tokens each at /4
        let cs = chunk_files(&files, 1500, &[]);
        assert_eq!(cs.len(), 2);
    }

    #[test]
    fn excluded_files_dropped() {
        let files = vec![file("docs/x.md", 100), file("src/y.rs", 100)];
        let cs = chunk_files(&files, 1000, &["docs/**".to_string()]);
        assert_eq!(cs[0].files.len(), 1);
        assert_eq!(cs[0].files[0].filename, "src/y.rs");
    }
}
