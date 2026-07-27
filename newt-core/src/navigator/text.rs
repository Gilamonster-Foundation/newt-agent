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
    text_search_scoped(query, root, None, index_id)
}

/// [`text_search`] with an optional workspace-relative `scope` (a file or a
/// directory). bug/steering-regressions iteration #6: the model narrows a
/// noisy search with `path` and the tool used to DROP the argument silently —
/// returning whole-workspace hits (tracked eval corpora included) against an
/// explicitly file-scoped query, then feeding the bait loop iteration #5
/// tagged. A scope is honored, fenced inside the workspace, and a missing or
/// escaping scope is an honest warning — never a silent widen.
#[must_use]
pub fn text_search_scoped(
    query: &str,
    workspace: &Path,
    scope: Option<&str>,
    index_id: &str,
) -> NavResult {
    let root = workspace;
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
    let scope = scope.map(str::trim).filter(|p| !p.is_empty());
    let search_root = match scope {
        Some(p)
            if Path::new(p).is_absolute()
                || Path::new(p)
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)) =>
        {
            result.complete = false;
            result.warnings.push(format!(
                "scope `{p}` must be a workspace-relative path without `..`"
            ));
            return result;
        }
        Some(p) => {
            let scoped = root.join(p);
            if !scoped.exists() {
                result.complete = false;
                result.warnings.push(format!(
                    "scope `{p}` does not exist in this workspace — nothing was \
                     searched (NOT a whole-workspace fallback)"
                ));
                return result;
            }
            scoped
        }
        None => root.to_path_buf(),
    };
    let mut hits = Vec::new();
    let searched = if search_root.is_file() {
        search_file(root, &search_root, &re, &mut hits)
    } else {
        search_dir(root, &search_root, &re, &mut hits)
    };
    if let Err(e) = searched {
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
    // bug/steering-regressions iteration #5: matches that live INSIDE string
    // literals are quoted message/fixture text, not code. Untagged, they bait
    // the model into chasing phantom files/symbols quoted by test fixtures
    // (every live drive orbited `help_sections.rs` this way). Tag each such
    // hit, and when an identifier-shaped query matches ONLY quoted text, say
    // so as a first-class verdict.
    let mut quoted_only_hits = 0usize;
    let total_hits = hits.len();
    for h in hits {
        let quoted = matches_only_inside_string_literals(&re, &h.line);
        if quoted {
            quoted_only_hits += 1;
        }
        result.hits.push(NavHit {
            path: h.path,
            start_line: h.line_number,
            end_line: h.line_number,
            kind: EvidenceKind::Lexical,
            snippet: h.line,
            symbol: None,
            detail: Some(if quoted {
                "text (inside a string literal — quoted text, not code)".into()
            } else {
                "text".into()
            }),
        });
    }
    let identifier_query = q.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && q.chars().next().is_some_and(|c| !c.is_ascii_digit());
    if identifier_query && total_hits > 0 && quoted_only_hits == total_hits {
        result.warnings.push(format!(
            "every match for `{q}` is inside string literals — quoted \
             message/fixture text, not code. `{q}` has NO code occurrence in \
             this workspace; do not treat quoted paths or symbols as real."
        ));
    }
    result
}

/// Does every `re` match on `line` fall inside a double-quoted string
/// literal? A cheap, language-agnostic heuristic (no parser): track `"…"`
/// spans honoring backslash escapes. Raw/multi-line strings whose quotes are
/// on other lines are NOT detected — the heuristic errs toward "code", which
/// is today's behavior, never toward hiding real code.
fn matches_only_inside_string_literals(re: &Regex, line: &str) -> bool {
    // Build the in-string byte spans for this line.
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => match start.take() {
                Some(s) => spans.push((s, idx)),
                None => start = Some(idx + ch.len_utf8()),
            },
            _ => {}
        }
    }
    if spans.is_empty() {
        return false;
    }
    let mut any = false;
    for m in re.find_iter(line) {
        any = true;
        let inside = spans.iter().any(|&(s, e)| m.start() >= s && m.end() <= e);
        if !inside {
            return false;
        }
    }
    any
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

    /// bug/steering-regressions iteration #5: fixture strings quoting phantom
    /// files/symbols must be tagged as quoted text, and an identifier query
    /// with ONLY quoted matches gets an explicit no-code-occurrence verdict —
    /// otherwise the model grounds its plan on bait (every live drive chased
    /// `help_sections.rs` out of a quoted compiler error).
    #[test]
    fn quoted_only_identifier_matches_get_a_no_code_occurrence_verdict() {
        let dir = std::env::temp_dir().join(format!(
            "newt-text-quoted-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("fixture.rs"),
            "fn t() { assert!(msg.contains(\"PHANTOM_TOKEN missing\"), \"{msg}\"); }\n",
        )
        .unwrap();
        let r = text_search("PHANTOM_TOKEN", &dir, "gen0");
        assert!(!r.hits.is_empty());
        assert!(
            r.hits.iter().all(|h| h
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("string literal"))),
            "quoted-only hits must be tagged: {:?}",
            r.hits
        );
        assert!(
            r.warnings.iter().any(|w| w.contains("NO code occurrence")),
            "identifier query with only quoted matches needs the verdict: {:?}",
            r.warnings
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// bug/steering-regressions iteration #6: a `path` scope must be HONORED
    /// (the tool silently dropped it, returning whole-workspace corpus noise
    /// against a file-scoped query), and a missing scope is an honest warning,
    /// never a silent whole-workspace fallback.
    #[test]
    fn scope_is_honored_and_missing_scope_is_an_honest_warning() {
        let dir = std::env::temp_dir().join(format!(
            "newt-text-scope-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::create_dir_all(dir.join("corpus")).unwrap();
        fs::write(dir.join("src/a.rs"), "fn wanted_here() {}\n").unwrap();
        fs::write(dir.join("corpus/junk.txt"), "wanted_here in noise\n").unwrap();

        // Directory scope: only src/ hits.
        let r = text_search_scoped("wanted_here", &dir, Some("src"), "gen0");
        assert!(
            r.hits.iter().all(|h| h.path.starts_with("src/")),
            "{:?}",
            r.hits
        );
        // File scope: exactly the one file, path still workspace-relative.
        let r = text_search_scoped("wanted_here", &dir, Some("src/a.rs"), "gen0");
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.hits[0].path, "src/a.rs");
        // Missing scope: honest refusal, no silent widen.
        let r = text_search_scoped("wanted_here", &dir, Some("no/such/dir"), "gen0");
        assert!(r.hits.is_empty());
        assert!(!r.complete);
        assert!(
            r.warnings.iter().any(|w| w.contains("does not exist")),
            "{:?}",
            r.warnings
        );
        // Escape attempts are refused.
        let r = text_search_scoped("wanted_here", &dir, Some("../elsewhere"), "gen0");
        assert!(!r.complete && r.hits.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_code_matches_are_not_tagged_as_quoted() {
        let dir = std::env::temp_dir().join(format!(
            "newt-text-code-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        // The symbol appears BOTH in code and in a string on another line:
        // the code hit stays untagged and no verdict fires.
        fs::write(
            dir.join("real.rs"),
            "const REAL_TOKEN: usize = 1;\nfn t() { assert!(m.contains(\"REAL_TOKEN\")); }\n",
        )
        .unwrap();
        let r = text_search("REAL_TOKEN", &dir, "gen0");
        assert!(r.hits.iter().any(|h| h.detail.as_deref() == Some("text")));
        assert!(
            !r.warnings.iter().any(|w| w.contains("NO code occurrence")),
            "a real code occurrence must never trigger the quoted-only verdict"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
