//! Lexical `/text` + `text_search` — regex-floor content search (#1387 Phase 2a).
//!
//! Mirrors the shape of `newt-tools::search` without taking a reverse dep on
//! that crate (newt-tools already depends on newt-core).

use std::path::Path;

use regex::Regex;

use crate::agentic::semantic::EvidenceKind;

use super::{NavHit, NavResult};

const MAX_HITS: usize = 200;

/// One lexical line hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextHit {
    pub path: String,
    pub line_number: usize,
    pub line: String,
}

/// Regex search under `root`, returned as a [`NavResult`] with `[LEXICAL]` labels.
#[must_use]
pub fn text_search(query: &str, root: &Path, index_id: &str) -> NavResult {
    let mut result = NavResult::empty(EvidenceKind::Lexical, "lexical-regex", index_id);
    let q = query.trim();
    if q.is_empty() {
        result.complete = false;
        result
            .warnings
            .push("text_search requires a non-empty query".into());
        return result;
    }
    let re = match Regex::new(q) {
        Ok(r) => r,
        Err(e) => {
            result.complete = false;
            result.warnings.push(format!("invalid regex: {e}"));
            return result;
        }
    };
    let mut hits = Vec::new();
    if let Err(e) = search_dir(root, root, &re, &mut hits) {
        result.complete = false;
        result.warnings.push(format!("search error: {e}"));
    }
    result.candidates = hits.len();
    if hits.len() >= MAX_HITS {
        result.complete = false;
        result
            .warnings
            .push(format!("hit cap {MAX_HITS} reached — results truncated"));
    }
    for h in hits {
        result.hits.push(NavHit {
            path: h.path,
            start_line: h.line_number,
            end_line: h.line_number,
            kind: EvidenceKind::Lexical,
            snippet: h.line,
            symbol: None,
            detail: Some("text".into()),
        });
    }
    result
}

fn search_dir(root: &Path, dir: &Path, re: &Regex, hits: &mut Vec<TextHit>) -> anyhow::Result<()> {
    if hits.len() >= MAX_HITS {
        return Ok(());
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if hits.len() >= MAX_HITS {
            break;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str == "target" || name_str == "node_modules" {
            continue;
        }
        if path.is_dir() {
            search_dir(root, &path, re, hits)?;
        } else if path.is_file() {
            search_file(root, &path, re, hits)?;
        }
    }
    Ok(())
}

fn search_file(
    root: &Path,
    path: &Path,
    re: &Regex,
    hits: &mut Vec<TextHit>,
) -> anyhow::Result<()> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    let rel = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    for (i, line) in content.lines().enumerate() {
        if hits.len() >= MAX_HITS {
            break;
        }
        if re.is_match(line) {
            hits.push(TextHit {
                path: rel.clone(),
                line_number: i + 1,
                line: line.to_string(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn lexical_hits_are_labelled() {
        let dir = std::env::temp_dir().join(format!(
            "newt-text-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.rs"), "fn hello() {}\nfn other() {}\n").unwrap();
        let r = text_search("hello", &dir, "gen0");
        assert_eq!(r.kind, EvidenceKind::Lexical);
        assert!(!r.hits.is_empty());
        assert!(r.render().contains("[LEXICAL]"));
        let _ = fs::remove_dir_all(&dir);
    }
}
