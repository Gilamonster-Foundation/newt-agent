//! Heuristic GRAPH ops — callers / callees / implementations / hierarchy
//! (#1387 Phase 2c). Analyzer is always `regex-floor`; `complete=false` when
//! weak.

use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};

use crate::agentic::semantic::EvidenceKind;

use super::{NavHit, NavResult};

/// Session GRAPH built from gathered sources (def table + call-name match +
/// Rust `impl … for …`).
#[derive(Debug, Clone, Default)]
pub struct GraphIndex {
    /// symbol → definition sites
    defs: BTreeMap<String, Vec<(String, usize, String)>>,
    /// file → source lines
    files: BTreeMap<String, Vec<String>>,
    /// trait → implementing types (path, line, type_name)
    impls_for: BTreeMap<String, Vec<(String, usize, String)>>,
    /// type → traits it implements
    type_impls: BTreeMap<String, Vec<(String, usize, String)>>,
    cuts_open: bool,
    index_id: String,
}

impl GraphIndex {
    #[must_use]
    pub fn build(files: &[(String, String)], cuts_open: bool, index_id: impl Into<String>) -> Self {
        let fn_re = Regex::new(
            r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)",
        )
        .unwrap();
        let py_def = Regex::new(r"(?m)^\s*def\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap();
        let impl_for = Regex::new(
            r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?impl(?:\s*<[^>]*>)?\s+([A-Za-z_][A-Za-z0-9_:]*)\s+for\s+([A-Za-z_][A-Za-z0-9_]*)",
        )
        .unwrap();
        let impl_type = Regex::new(
            r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?impl(?:\s*<[^>]*>)?\s+([A-Za-z_][A-Za-z0-9_]*)\s*\{",
        )
        .unwrap();
        let mut defs: BTreeMap<String, Vec<(String, usize, String)>> = BTreeMap::new();
        let mut file_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut impls_for: BTreeMap<String, Vec<(String, usize, String)>> = BTreeMap::new();
        let mut type_impls: BTreeMap<String, Vec<(String, usize, String)>> = BTreeMap::new();

        for (path, src) in files {
            let lines: Vec<String> = src.lines().map(str::to_string).collect();
            for (i, line) in lines.iter().enumerate() {
                let lineno = i + 1;
                if let Some(c) = fn_re.captures(line) {
                    defs.entry(c[1].to_string()).or_default().push((
                        path.clone(),
                        lineno,
                        line.clone(),
                    ));
                }
                if let Some(c) = py_def.captures(line) {
                    defs.entry(c[1].to_string()).or_default().push((
                        path.clone(),
                        lineno,
                        line.clone(),
                    ));
                }
                if let Some(c) = impl_for.captures(line) {
                    let trait_name = c[1].to_string();
                    let type_name = c[2].to_string();
                    impls_for.entry(trait_name.clone()).or_default().push((
                        path.clone(),
                        lineno,
                        type_name.clone(),
                    ));
                    type_impls.entry(type_name).or_default().push((
                        path.clone(),
                        lineno,
                        trait_name,
                    ));
                } else if let Some(c) = impl_type.captures(line) {
                    // Inherent impl — record under type name for hierarchy.
                    type_impls.entry(c[1].to_string()).or_default().push((
                        path.clone(),
                        lineno,
                        "(inherent)".into(),
                    ));
                }
            }
            file_map.insert(path.clone(), lines);
        }
        Self {
            defs,
            files: file_map,
            impls_for,
            type_impls,
            cuts_open,
            index_id: index_id.into(),
        }
    }

    #[must_use]
    pub fn index_id(&self) -> &str {
        &self.index_id
    }

    fn base_result(&self, weak: bool) -> NavResult {
        let mut r = NavResult::empty(EvidenceKind::Graph, "regex-floor", &self.index_id);
        r.complete = !self.cuts_open && !weak;
        r.warnings.push(
            "GRAPH edges are regex-floor heuristics (call-name match / impl…for…), not a typechecker"
                .into(),
        );
        if self.cuts_open {
            r.warnings
                .push("gather had cuts — graph may be incomplete".into());
        }
        if weak {
            r.warnings
                .push("weak GRAPH confidence — complete=false".into());
        }
        r
    }
}

/// Call sites that mention `symbol(` (excluding its own definition lines).
#[must_use]
pub fn find_callers(index: &GraphIndex, symbol: &str) -> NavResult {
    let sym = symbol.trim();
    let weak = !index.defs.contains_key(sym);
    let mut result = index.base_result(true);
    result.complete = false; // call-name match is always weak
    if weak {
        result.warnings.push(format!(
            "`{sym}` not in GRAPH def table — callers are especially weak"
        ));
    }
    let call_pat = format!(r"\b{}\s*\(", regex::escape(sym));
    let re = match Regex::new(&call_pat) {
        Ok(r) => r,
        Err(_) => return result,
    };
    let def_lines: BTreeSet<(String, usize)> = index
        .defs
        .get(sym)
        .into_iter()
        .flatten()
        .map(|(p, l, _)| (p.clone(), *l))
        .collect();
    let mut candidates = 0usize;
    for (path, lines) in &index.files {
        for (i, line) in lines.iter().enumerate() {
            let lineno = i + 1;
            if def_lines.contains(&(path.clone(), lineno)) {
                continue;
            }
            if re.is_match(line) {
                candidates += 1;
                result.hits.push(NavHit {
                    path: path.clone(),
                    start_line: lineno,
                    end_line: lineno,
                    kind: EvidenceKind::Graph,
                    snippet: line.clone(),
                    symbol: Some(sym.to_string()),
                    detail: Some("caller".into()),
                });
            }
        }
    }
    result.candidates = candidates;
    result
}

/// Names called inside the body of `symbol`'s definition (next fn boundary).
#[must_use]
pub fn find_callees(index: &GraphIndex, symbol: &str) -> NavResult {
    let sym = symbol.trim();
    let mut result = index.base_result(true);
    result.complete = false;
    let Some(defs) = index.defs.get(sym) else {
        result
            .warnings
            .push(format!("no definition for `{sym}` in GRAPH def table"));
        return result;
    };
    let call_re = Regex::new(r"\b([A-Za-z_][A-Za-z0-9_]*)\s*\(").unwrap();
    let mut seen = BTreeSet::new();
    for (path, start, _) in defs {
        let Some(lines) = index.files.get(path) else {
            continue;
        };
        let start_idx = start.saturating_sub(1);
        // Scan until next top-level fn / end of file (heuristic).
        let mut end_idx = lines.len();
        for (i, line) in lines.iter().enumerate().skip(start_idx + 1) {
            let t = line.trim_start();
            if (t.starts_with("fn ") || t.starts_with("pub fn ") || t.starts_with("async fn "))
                && !t.contains(&format!("fn {sym}"))
            {
                end_idx = i;
                break;
            }
        }
        for (i, line) in lines.iter().enumerate().take(end_idx).skip(start_idx) {
            for cap in call_re.captures_iter(line) {
                let name = &cap[1];
                if name == sym
                    || matches!(
                        name,
                        "if" | "for"
                            | "while"
                            | "match"
                            | "Some"
                            | "Ok"
                            | "Err"
                            | "vec"
                            | "format"
                            | "println"
                            | "eprintln"
                            | "assert"
                            | "panic"
                            | "todo"
                            | "unimplemented"
                    )
                {
                    continue;
                }
                if seen.insert(name.to_string()) {
                    result.hits.push(NavHit {
                        path: path.clone(),
                        start_line: i + 1,
                        end_line: i + 1,
                        kind: EvidenceKind::Graph,
                        snippet: line.clone(),
                        symbol: Some(name.to_string()),
                        detail: Some("callee".into()),
                    });
                }
            }
        }
    }
    result.candidates = result.hits.len();
    result
}

/// `impl Trait for Type` rows for `symbol` (as trait or type).
#[must_use]
pub fn find_implementations(index: &GraphIndex, symbol: &str) -> NavResult {
    let sym = symbol.trim();
    let mut result = index.base_result(false);
    // impl…for… is stronger than bare call-name, but still regex-floor.
    if index.cuts_open
        || (!index.impls_for.contains_key(sym) && !index.type_impls.contains_key(sym))
    {
        result.complete = false;
        if !index.impls_for.contains_key(sym) && !index.type_impls.contains_key(sym) {
            result.warnings.push(
                "no impl…for… rows matched — complete=false (regex-floor may have missed)".into(),
            );
        }
    }
    if let Some(rows) = index.impls_for.get(sym) {
        for (path, line, ty) in rows {
            result.hits.push(NavHit {
                path: path.clone(),
                start_line: *line,
                end_line: *line,
                kind: EvidenceKind::Graph,
                snippet: format!("impl {sym} for {ty}"),
                symbol: Some(ty.clone()),
                detail: Some("impl_for".into()),
            });
        }
    }
    if let Some(rows) = index.type_impls.get(sym) {
        for (path, line, trait_name) in rows {
            result.hits.push(NavHit {
                path: path.clone(),
                start_line: *line,
                end_line: *line,
                kind: EvidenceKind::Graph,
                snippet: format!("impl {trait_name} for {sym}"),
                symbol: Some(trait_name.clone()),
                detail: Some("type_impl".into()),
            });
        }
    }
    result.candidates = result.hits.len();
    result
}

/// Hierarchy: traits a type implements, or types that implement a trait.
#[must_use]
pub fn find_hierarchy(index: &GraphIndex, symbol: &str) -> NavResult {
    let mut result = find_implementations(index, symbol);
    result.analyzer = "regex-floor".into();
    result.warnings.push(
        "hierarchy is impl…for… projection only — no inheritance / supertrait expansion".into(),
    );
    for hit in &mut result.hits {
        hit.detail = Some("hierarchy".into());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> GraphIndex {
        let files = vec![(
            "lib.rs".into(),
            "fn foo() {}\nfn bar() { foo(); }\npub trait T {}\nimpl T for S {}\nstruct S;\n".into(),
        )];
        GraphIndex::build(&files, false, "gen1")
    }

    #[test]
    fn callers_are_graph_incomplete() {
        let idx = sample();
        let r = find_callers(&idx, "foo");
        assert_eq!(r.kind, EvidenceKind::Graph);
        assert!(!r.complete);
        assert!(r.analyzer.contains("regex-floor"));
        assert!(r.hits.iter().any(|h| h.detail.as_deref() == Some("caller")));
    }

    #[test]
    fn implementations_find_impl_for() {
        let idx = sample();
        let r = find_implementations(&idx, "T");
        assert!(!r.hits.is_empty());
        assert!(r.render().contains("[GRAPH]"));
    }

    #[test]
    fn callees_from_body() {
        let idx = sample();
        let r = find_callees(&idx, "bar");
        assert!(!r.complete);
        assert!(r.hits.iter().any(|h| h.symbol.as_deref() == Some("foo")));
    }
}
