//! `/type` inspect_type + `/impact` (#1387 Phase 4). Heuristic only — not a
//! typechecker. Persistent index (`#1282`) and full dataflow are omitted.

use std::path::Path;

use regex::Regex;

use crate::agentic::semantic::EvidenceKind;
use crate::project_model::ProjectModel;
use crate::symbols::{extract_definitions, extract_references, DefKind, Lang};
use crate::where_is::WhereIsIndex;

use super::{NavHit, NavResult};

/// Result of a heuristic type/symbol inspect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeInspect {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub line: usize,
    pub snippet: String,
}

impl TypeInspect {
    #[must_use]
    pub fn to_nav(&self, index_id: &str) -> NavResult {
        let mut r = NavResult::empty(EvidenceKind::Symbol, "inspect-heuristic", index_id);
        r.complete = false; // never typechecker-proved
        r.warnings.push(
            "inspect_type is NOT typechecker-proved — kind/snippet are regex-floor extraction"
                .into(),
        );
        r.candidates = 1;
        r.hits.push(NavHit {
            path: self.path.clone(),
            start_line: self.line,
            end_line: self.line,
            kind: EvidenceKind::Symbol,
            snippet: self.snippet.clone(),
            symbol: Some(self.name.clone()),
            detail: Some(self.kind.clone()),
        });
        r
    }
}

/// Inspect a symbol: defining snippet + kind + file:line.
#[must_use]
pub fn inspect_type(
    symbol: &str,
    files: &[(String, String)],
    where_is: Option<&WhereIsIndex>,
    index_id: &str,
) -> NavResult {
    let sym = symbol.trim();
    let mut result = NavResult::empty(EvidenceKind::Symbol, "inspect-heuristic", index_id);
    result.complete = false;
    result.warnings.push(
        "inspect_type is NOT typechecker-proved — kind/snippet are regex-floor extraction".into(),
    );
    if sym.is_empty() {
        result.warnings.push("inspect_type needs a symbol".into());
        return result;
    }

    // Prefer where_is path, then scan definitions.
    if let Some(idx) = where_is {
        if let crate::where_is::LookupVerdict::Found { witnesses } = idx.where_is(sym, None) {
            for w in witnesses {
                if let Some((_, src)) = files
                    .iter()
                    .find(|(p, _)| p == &w.path || p.ends_with(&w.path))
                {
                    let (line, snippet) =
                        find_def_line(src, sym).unwrap_or((1, format!("{} ({})", sym, w.kind)));
                    result.hits.push(NavHit {
                        path: w.path.clone(),
                        start_line: line,
                        end_line: line,
                        kind: EvidenceKind::Symbol,
                        snippet,
                        symbol: Some(sym.to_string()),
                        detail: Some(w.kind.clone()),
                    });
                } else {
                    result.hits.push(NavHit {
                        path: w.path,
                        start_line: 1,
                        end_line: 1,
                        kind: EvidenceKind::Symbol,
                        snippet: format!("{sym} ({})", w.kind),
                        symbol: Some(sym.to_string()),
                        detail: Some(w.kind),
                    });
                }
            }
            result.candidates = result.hits.len();
            return result;
        }
    }

    for (path, src) in files {
        let lang = if path.ends_with(".py") {
            Lang::Python
        } else {
            Lang::Rust
        };
        for d in extract_definitions(src, lang) {
            if d.name == sym {
                let snippet = src
                    .lines()
                    .nth(d.line.saturating_sub(1))
                    .unwrap_or("")
                    .to_string();
                let kind = match d.kind {
                    DefKind::Function => "function",
                    DefKind::Class => "class",
                    DefKind::Struct => "struct",
                    DefKind::Enum => "enum",
                    DefKind::Trait => "trait",
                };
                result.hits.push(NavHit {
                    path: path.clone(),
                    start_line: d.line,
                    end_line: d.line,
                    kind: EvidenceKind::Symbol,
                    snippet,
                    symbol: Some(sym.to_string()),
                    detail: Some(kind.into()),
                });
            }
        }
    }
    result.candidates = result.hits.len();
    if result.hits.is_empty() {
        result
            .warnings
            .push(format!("no defining snippet found for `{sym}`"));
    }
    result
}

/// `/map` → curated [`NavResult`] over project-model units (#1387 Phase 2b).
#[must_use]
pub fn project_map_nav(model: &ProjectModel, expand: Option<&str>, index_id: &str) -> NavResult {
    let mut r = NavResult::empty(EvidenceKind::Curated, "project-map", index_id);
    for u in &model.units {
        let focused = expand.is_some_and(|e| e == u.name || e == u.dir);
        let snippet = if focused {
            format!(
                "expanded `{}`: dir={} roots={:?} deps={:?} langs={:?}",
                u.name, u.dir, u.source_roots, u.deps, u.languages
            )
        } else {
            format!(
                "{} · {} deps · langs={:?}",
                if u.source_roots.is_empty() {
                    String::new()
                } else {
                    format!("[{}]", u.source_roots.join(", "))
                },
                u.deps.len(),
                u.languages
            )
        };
        r.hits.push(NavHit {
            path: u.dir.clone(),
            start_line: 1,
            end_line: 1,
            kind: EvidenceKind::Curated,
            snippet,
            symbol: Some(u.name.clone()),
            detail: Some(if focused {
                "expanded".into()
            } else {
                "unit".into()
            }),
        });
    }
    r.candidates = r.hits.len();
    if let Some(unit) = expand {
        if !model.units.iter().any(|u| u.name == unit || u.dir == unit) {
            r.warnings.push(format!("no unit named `{unit}`"));
        }
    }
    if model.units.is_empty() {
        r.warnings.push("empty project model".into());
    }
    r
}

fn find_def_line(src: &str, sym: &str) -> Option<(usize, String)> {
    let patterns = [
        format!(r"\bfn\s+{sym}\b"),
        format!(r"\bstruct\s+{sym}\b"),
        format!(r"\benum\s+{sym}\b"),
        format!(r"\btrait\s+{sym}\b"),
        format!(r"\btype\s+{sym}\b"),
        format!(r"\bclass\s+{sym}\b"),
        format!(r"\bdef\s+{sym}\b"),
    ];
    for (i, line) in src.lines().enumerate() {
        for p in &patterns {
            if let Ok(re) = Regex::new(p) {
                if re.is_match(line) {
                    return Some((i + 1, line.to_string()));
                }
            }
        }
    }
    None
}

/// Impact report: outbound deps + reverse deps from [`ProjectModel`], optional lcov.
#[derive(Debug, Clone, PartialEq)]
pub struct ImpactReport {
    pub unit: String,
    pub outbound: Vec<String>,
    pub reverse: Vec<String>,
    pub importers: Vec<(String, usize)>,
    pub lcov_lines: Option<Vec<(String, u32, u32)>>,
    pub complete: bool,
    pub warnings: Vec<String>,
}

impl ImpactReport {
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!(
            "[CURATED]/] impact for `{}`  complete={}\n",
            self.unit, self.complete
        );
        // Use SYMBOL-ish framing; impact is project-model derived.
        out = out.replace("[CURATED COP]", EvidenceKind::Curated.label());
        out.push_str(&format!("  outbound deps ({}):\n", self.outbound.len()));
        for d in &self.outbound {
            out.push_str(&format!("    → {d}\n"));
        }
        out.push_str(&format!("  reverse deps ({}):\n", self.reverse.len()));
        for d in &self.reverse {
            out.push_str(&format!("    ← {d}\n"));
        }
        if !self.importers.is_empty() {
            out.push_str(&format!("  module importers ({}):\n", self.importers.len()));
            for (path, line) in &self.importers {
                out.push_str(&format!("    {path}:{line}\n"));
            }
        }
        match &self.lcov_lines {
            Some(rows) => {
                out.push_str(&format!("  lcov join ({} file rows):\n", rows.len()));
                for (path, hit, found) in rows.iter().take(20) {
                    out.push_str(&format!("    {path}  hit={hit} found={found}\n"));
                }
            }
            None => out.push_str("  lcov: (no lcov.info — coverage join incomplete)\n"),
        }
        for w in &self.warnings {
            out.push_str(&format!("  warning: {w}\n"));
        }
        out
    }

    #[must_use]
    pub fn to_nav(&self, index_id: &str) -> NavResult {
        let mut r = NavResult::empty(EvidenceKind::Curated, "project-model", index_id);
        r.complete = self.complete;
        r.warnings.extend(self.warnings.clone());
        for d in &self.outbound {
            r.hits.push(NavHit {
                path: d.clone(),
                start_line: 1,
                end_line: 1,
                kind: EvidenceKind::Curated,
                snippet: format!("outbound dep: {d}"),
                symbol: Some(self.unit.clone()),
                detail: Some("outbound".into()),
            });
        }
        for d in &self.reverse {
            r.hits.push(NavHit {
                path: d.clone(),
                start_line: 1,
                end_line: 1,
                kind: EvidenceKind::Curated,
                snippet: format!("reverse dep: {d}"),
                symbol: Some(self.unit.clone()),
                detail: Some("reverse".into()),
            });
        }
        r.candidates = r.hits.len();
        r
    }
}

/// Compute impact for a unit/crate name from the project model (+ optional refs).
#[must_use]
pub fn impact_analysis(
    unit_or_symbol: &str,
    model: &ProjectModel,
    files: &[(String, String)],
    workspace: &Path,
) -> ImpactReport {
    let name = unit_or_symbol.trim();
    let mut warnings = vec![
        "impact uses ProjectModel deps + regex import extraction — not a full call graph".into(),
        "persistent content-hash index (#1282) is out of scope for this path".into(),
    ];
    let unit = model
        .units
        .iter()
        .find(|u| u.name == name || u.dir == name)
        .cloned();
    let (outbound, reverse) = match &unit {
        Some(u) => {
            let outbound = u.deps.clone();
            let reverse: Vec<String> = model
                .units
                .iter()
                .filter(|other| other.deps.iter().any(|d| d == &u.name || d == name))
                .map(|other| other.name.clone())
                .collect();
            (outbound, reverse)
        }
        None => {
            warnings.push(format!(
                "no project unit named `{name}` — outbound/reverse empty; trying import scan"
            ));
            (Vec::new(), Vec::new())
        }
    };

    // Module importers via extract_references.
    let mut importers = Vec::new();
    for (path, src) in files {
        let lang = if path.ends_with(".py") {
            Lang::Python
        } else {
            Lang::Rust
        };
        for r in extract_references(src, lang) {
            let hay = format!("{}{}", r.module, r.name.as_deref().unwrap_or(""));
            if hay.contains(name) || r.module.split("::").any(|p| p == name) {
                importers.push((path.clone(), r.line));
            }
        }
    }
    importers.sort();
    importers.dedup();

    let lcov_path = workspace.join("lcov.info");
    let lcov_lines = if lcov_path.is_file() {
        Some(parse_lcov_summary(
            &lcov_path,
            unit.as_ref().map(|u| u.dir.as_str()),
        ))
    } else {
        warnings.push("lcov.info not present — coverage join incomplete".into());
        None
    };

    let complete = unit.is_some() && warnings.iter().all(|w| !w.contains("incomplete"));
    ImpactReport {
        unit: name.to_string(),
        outbound,
        reverse,
        importers,
        lcov_lines,
        complete,
        warnings,
    }
}

/// Minimal lcov summary: per-file LH/LF for paths under `dir_filter`.
fn parse_lcov_summary(path: &Path, dir_filter: Option<&str>) -> Vec<(String, u32, u32)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cur_file = String::new();
    let mut lh = 0u32;
    let mut lf = 0u32;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("SF:") {
            if !cur_file.is_empty() && dir_filter.is_none_or(|d| d == "." || cur_file.contains(d)) {
                out.push((cur_file.clone(), lh, lf));
            }
            cur_file = rest.to_string();
            lh = 0;
            lf = 0;
        } else if let Some(rest) = line.strip_prefix("LH:") {
            lh = rest.parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("LF:") {
            lf = rest.parse().unwrap_or(0);
        } else if line.trim() == "end_of_record" {
            if !cur_file.is_empty() && dir_filter.is_none_or(|d| d == "." || cur_file.contains(d)) {
                out.push((cur_file.clone(), lh, lf));
            }
            cur_file.clear();
            lh = 0;
            lf = 0;
        }
    }
    // Cap for display.
    out.truncate(50);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_model::ProjectUnit;

    #[test]
    fn inspect_warns_not_proved() {
        let files = vec![("a.rs".into(), "pub struct Foo;\n".into())];
        let r = inspect_type("Foo", &files, None, "gen1");
        assert!(!r.complete);
        assert!(r.warnings.iter().any(|w| w.contains("NOT typechecker")));
        assert!(!r.hits.is_empty());
    }

    #[test]
    fn project_map_nav_is_curated() {
        let model = ProjectModel {
            pack: "rust".into(),
            units: vec![ProjectUnit {
                name: "core".into(),
                dir: "core".into(),
                source_roots: vec!["src".into()],
                deps: vec![],
                languages: vec!["rust".into()],
            }],
        };
        let r = project_map_nav(&model, Some("core"), "gen1");
        assert_eq!(r.kind, EvidenceKind::Curated);
        assert_eq!(r.analyzer, "project-map");
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.hits[0].detail.as_deref(), Some("expanded"));
        assert!(r.hits[0].snippet.contains("expanded"));
    }

    #[test]
    fn impact_reverse_deps() {
        let model = ProjectModel {
            pack: "rust".into(),
            units: vec![
                ProjectUnit {
                    name: "core".into(),
                    dir: "core".into(),
                    source_roots: vec!["src".into()],
                    deps: vec![],
                    languages: vec!["rust".into()],
                },
                ProjectUnit {
                    name: "tui".into(),
                    dir: "tui".into(),
                    source_roots: vec!["src".into()],
                    deps: vec!["core".into()],
                    languages: vec!["rust".into()],
                },
            ],
        };
        let report = impact_analysis("core", &model, &[], Path::new("/tmp"));
        assert!(report.reverse.iter().any(|r| r == "tui"));
        assert!(report.render().contains("reverse"));
    }
}
