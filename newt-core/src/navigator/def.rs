//! `/def` + `goto_definition` — wraps `where_is` with [`NavResult`] honesty.

use crate::agentic::semantic::EvidenceKind;
use crate::symbols::{extract_definitions, Lang};
use crate::where_is::{LookupVerdict, WhereIsIndex};

use super::{NavHit, NavResult};

#[derive(Debug, Clone, Default)]
pub struct GotoDefinitionArgs<'a> {
    pub symbol: &'a str,
    pub kind: Option<&'a str>,
    pub index_id: &'a str,
    /// Session files — when present, resolve real def line numbers via
    /// [`extract_definitions`] instead of stubbing `1-1`.
    pub files: Option<&'a [(String, String)]>,
}

/// Resolve a symbol definition into a [`NavResult`] (`[SYMBOL]`).
#[must_use]
pub fn goto_definition(index: &WhereIsIndex, args: GotoDefinitionArgs<'_>) -> NavResult {
    let mut result = NavResult::empty(EvidenceKind::Symbol, "where_is", args.index_id);
    let symbol = args.symbol.trim();
    if symbol.is_empty() {
        result.complete = false;
        result
            .warnings
            .push("goto_definition requires a non-empty symbol".into());
        return result;
    }
    result.candidates = 1;
    match index.where_is(symbol, args.kind) {
        LookupVerdict::Found { witnesses } => {
            result.complete = index.cut_free();
            if !result.complete {
                result.warnings.push(
                    "gather had open cuts — additional definitions may exist in cut files".into(),
                );
            }
            for w in witnesses {
                let (start_line, end_line, snippet) =
                    resolve_def_span(args.files, &w.path, symbol, &w.kind);
                result.hits.push(NavHit {
                    path: w.path,
                    start_line,
                    end_line,
                    kind: EvidenceKind::Symbol,
                    snippet,
                    symbol: Some(symbol.to_string()),
                    detail: Some(w.kind),
                });
            }
        }
        LookupVerdict::NotGathered { cuts_open } => {
            result.complete = false;
            result.warnings.push(format!(
                "NOT_GATHERED — open cut classes: [{}]; miss is not proof of absence",
                cuts_open.join(", ")
            ));
        }
        LookupVerdict::NoSuchSymbol => {
            result.complete = true;
            result
                .warnings
                .push("NO_SUCH_SYMBOL — complete index has no definition by that name".into());
        }
    }
    result
}

fn resolve_def_span(
    files: Option<&[(String, String)]>,
    path: &str,
    symbol: &str,
    kind: &str,
) -> (usize, usize, String) {
    let fallback = (1, 1, format!("{symbol} ({kind})"));
    let Some(files) = files else {
        return fallback;
    };
    let Some((_, src)) = files
        .iter()
        .find(|(p, _)| p == path || p.ends_with(path) || path.ends_with(p.as_str()))
    else {
        return fallback;
    };
    let lang = if path.ends_with(".py") {
        Lang::Python
    } else {
        Lang::Rust
    };
    if let Some(d) = extract_definitions(src, lang)
        .into_iter()
        .find(|d| d.name == symbol)
    {
        let snippet = src
            .lines()
            .nth(d.line.saturating_sub(1))
            .unwrap_or("")
            .to_string();
        return (d.line, d.line, snippet);
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::where_is::WhereIsIndex;

    #[test]
    fn found_is_symbol_kind() {
        let idx =
            WhereIsIndex::from_facts(vec![("run".into(), "a.rs".into(), "fn".into())], vec![]);
        let r = goto_definition(
            &idx,
            GotoDefinitionArgs {
                symbol: "run",
                kind: None,
                index_id: "gen1",
                files: None,
            },
        );
        assert_eq!(r.kind, EvidenceKind::Symbol);
        assert!(r.complete);
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.hits[0].path, "a.rs");
    }

    #[test]
    fn found_with_files_gets_real_line() {
        let src = "// header\npub fn run() {}\n";
        let files = [("a.rs".into(), src.into())];
        let idx =
            WhereIsIndex::from_facts(vec![("run".into(), "a.rs".into(), "fn".into())], vec![]);
        let r = goto_definition(
            &idx,
            GotoDefinitionArgs {
                symbol: "run",
                kind: None,
                index_id: "gen1",
                files: Some(&files),
            },
        );
        assert_eq!(r.hits[0].start_line, 2);
        assert_eq!(r.hits[0].end_line, 2);
        assert!(r.hits[0].snippet.contains("fn run"));
        assert_eq!(r.hits[0].loc_key(), "a.rs:2-2");
    }

    #[test]
    fn not_gathered_is_incomplete() {
        let idx = WhereIsIndex::from_facts(vec![], vec!["too_large".into()]);
        let r = goto_definition(
            &idx,
            GotoDefinitionArgs {
                symbol: "missing",
                kind: None,
                index_id: "gen1",
                files: None,
            },
        );
        assert!(!r.complete);
        assert!(r.hits.is_empty());
    }
}
