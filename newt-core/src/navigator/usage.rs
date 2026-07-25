//! Session [`UsageIndex`] — heuristic find-references / find-tests (#1387 Phase 2b).

use regex::Regex;
use std::collections::BTreeMap;

use crate::agentic::semantic::EvidenceKind;

use super::{NavHit, NavResult};

/// One usage / reference site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageSite {
    pub path: String,
    pub line: usize,
    pub kind: String,
    pub snippet: String,
}

/// Session-scoped name→sites table built from gathered sources (regex-floor).
#[derive(Debug, Clone, Default)]
pub struct UsageIndex {
    by_symbol: BTreeMap<String, Vec<UsageSite>>,
    cuts_open: bool,
    index_id: String,
    /// Paths that look like test files (for `/tests`).
    test_files: Vec<String>,
}

impl UsageIndex {
    /// Build from `(path, source)` pairs. `cuts_open` forces `complete=false`.
    #[must_use]
    pub fn build(files: &[(String, String)], cuts_open: bool, index_id: impl Into<String>) -> Self {
        let ident = Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").unwrap();
        let mut by_symbol: BTreeMap<String, Vec<UsageSite>> = BTreeMap::new();
        let mut test_files = Vec::new();
        for (path, src) in files {
            let path_lc = path.to_lowercase();
            if path_lc.contains("test")
                || path_lc.ends_with("_test.rs")
                || path_lc.ends_with("_test.py")
            {
                test_files.push(path.clone());
            }
            for (i, line) in src.lines().enumerate() {
                let lineno = i + 1;
                let trimmed = line.trim();
                if trimmed.starts_with("//") || trimmed.starts_with('#') {
                    continue;
                }
                for m in ident.find_iter(line) {
                    let name = m.as_str();
                    if name.len() < 2 {
                        continue;
                    }
                    // Skip common keywords noise.
                    if matches!(
                        name,
                        "fn" | "let"
                            | "mut"
                            | "pub"
                            | "use"
                            | "mod"
                            | "impl"
                            | "struct"
                            | "enum"
                            | "trait"
                            | "self"
                            | "Self"
                            | "super"
                            | "crate"
                            | "return"
                            | "if"
                            | "else"
                            | "match"
                            | "for"
                            | "while"
                            | "loop"
                            | "in"
                            | "as"
                            | "const"
                            | "static"
                            | "type"
                            | "where"
                            | "async"
                            | "await"
                            | "move"
                            | "ref"
                            | "true"
                            | "false"
                            | "def"
                            | "class"
                            | "from"
                            | "import"
                            | "None"
                            | "True"
                            | "False"
                            | "and"
                            | "or"
                            | "not"
                    ) {
                        continue;
                    }
                    let kind = if line.contains(&format!("{name}(")) {
                        "call_or_ref"
                    } else if trimmed.contains(&format!("fn {name}"))
                        || trimmed.contains(&format!("def {name}"))
                        || trimmed.contains(&format!("struct {name}"))
                        || trimmed.contains(&format!("class {name}"))
                    {
                        "definition"
                    } else {
                        "mention"
                    };
                    by_symbol
                        .entry(name.to_string())
                        .or_default()
                        .push(UsageSite {
                            path: path.clone(),
                            line: lineno,
                            kind: kind.into(),
                            snippet: line.to_string(),
                        });
                }
            }
        }
        for sites in by_symbol.values_mut() {
            sites.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
            sites.dedup_by(|a, b| a.path == b.path && a.line == b.line);
        }
        test_files.sort();
        test_files.dedup();
        Self {
            by_symbol,
            cuts_open,
            index_id: index_id.into(),
            test_files,
        }
    }

    #[must_use]
    pub fn index_id(&self) -> &str {
        &self.index_id
    }
}

/// Find references / uses of `symbol` (heuristic name match).
#[must_use]
pub fn find_references(index: &UsageIndex, symbol: &str) -> NavResult {
    let mut result = NavResult::empty(EvidenceKind::Symbol, "usage-index", &index.index_id);
    let sym = symbol.trim();
    if sym.is_empty() {
        result.complete = false;
        result
            .warnings
            .push("find_references needs a symbol".into());
        return result;
    }
    result.complete = !index.cuts_open;
    if index.cuts_open {
        result
            .warnings
            .push("gather had cuts — reference set may be incomplete".into());
    }
    result
        .warnings
        .push("USAGE sites are regex-floor name matches, not compiler-resolved references".into());
    let Some(sites) = index.by_symbol.get(sym) else {
        result.candidates = 0;
        return result;
    };
    result.candidates = sites.len();
    for site in sites {
        // Skip pure definition lines when looking for "uses" — keep them but
        // label; callers often want both.
        result.hits.push(NavHit {
            path: site.path.clone(),
            start_line: site.line,
            end_line: site.line,
            kind: EvidenceKind::Symbol,
            snippet: site.snippet.clone(),
            symbol: Some(sym.to_string()),
            detail: Some(site.kind.clone()),
        });
    }
    result
}

/// Heuristic test discovery for a symbol (name in test paths / `#[test]` files).
#[must_use]
pub fn find_tests(index: &UsageIndex, symbol: &str) -> NavResult {
    let mut result = NavResult::empty(EvidenceKind::Symbol, "usage-tests", &index.index_id);
    let sym = symbol.trim();
    result.complete = !index.cuts_open;
    result
        .warnings
        .push("test discovery is path/name heuristic — not a test-runner inventory".into());
    if index.cuts_open {
        result
            .warnings
            .push("gather had cuts — test set may be incomplete".into());
    }
    let mut candidates = 0usize;
    if let Some(sites) = index.by_symbol.get(sym) {
        for site in sites {
            let path_lc = site.path.to_lowercase();
            let is_test = path_lc.contains("test")
                || site.snippet.contains("#[test]")
                || site.snippet.contains("def test_");
            if is_test {
                candidates += 1;
                result.hits.push(NavHit {
                    path: site.path.clone(),
                    start_line: site.line,
                    end_line: site.line,
                    kind: EvidenceKind::Symbol,
                    snippet: site.snippet.clone(),
                    symbol: Some(sym.to_string()),
                    detail: Some("test".into()),
                });
            }
        }
    }
    // Also list test files that mention the symbol nowhere but match by name.
    for path in &index.test_files {
        if path.to_lowercase().contains(&sym.to_lowercase())
            && !result.hits.iter().any(|h| &h.path == path)
        {
            candidates += 1;
            result.hits.push(NavHit {
                path: path.clone(),
                start_line: 1,
                end_line: 1,
                kind: EvidenceKind::Symbol,
                snippet: format!("test file matching `{sym}`"),
                symbol: Some(sym.to_string()),
                detail: Some("test_file".into()),
            });
        }
    }
    result.candidates = candidates;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_find_call_sites() {
        let files = vec![
            ("lib.rs".into(), "fn foo() {}\n".into()),
            ("main.rs".into(), "fn main() { foo(); }\n".into()),
        ];
        let idx = UsageIndex::build(&files, false, "gen1");
        let r = find_references(&idx, "foo");
        assert!(r.hits.len() >= 2);
        assert!(r.complete);
        assert!(r.render().contains("[SYMBOL]"));
    }

    #[test]
    fn cuts_make_incomplete() {
        let idx = UsageIndex::build(&[], true, "gen1");
        let r = find_references(&idx, "x");
        assert!(!r.complete);
    }
}
