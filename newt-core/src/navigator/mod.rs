//! Code Navigator (#1387) — shared repository-intelligence service.
//!
//! Human slash commands, agent tools, and auto-inject share one API. Every
//! miss reports completeness; `[SEMANTIC]` is never claimed as structural
//! proof. Structural ops use in-process regex-floor heuristics with loud
//! `complete=false` / analyzer warnings. Persistent content-hash indexing
//! stays deferred to `#1282`.

mod def;
mod graph;
mod impact;
mod ledger;
mod text;
mod tools;
mod usage;

pub use def::{goto_definition, GotoDefinitionArgs};
pub use graph::{find_callees, find_callers, find_hierarchy, find_implementations, GraphIndex};
pub use impact::{impact_analysis, inspect_type, project_map_nav, ImpactReport, TypeInspect};
pub use ledger::{
    compare_ledgers, compare_semantic_lexical, export_ledger_json, export_ledger_markdown,
    format_ledger_diff, format_ledger_human, format_ledger_model, hash_context, RetrievalLedger,
    TurnRetrieval,
};
pub use text::{text_search, TextHit};
pub use tools::{
    execute_nav_tool, find_callees_tool_definition, find_callers_tool_definition,
    find_hierarchy_tool_definition, find_implementations_tool_definition,
    find_references_tool_definition, find_tests_tool_definition, goto_definition_tool_definition,
    impact_tool_definition, inspect_type_tool_definition, text_search_tool_definition, NavToolCtx,
    NAV_TOOL_NAMES,
};
pub use usage::{find_references, find_tests, UsageIndex, UsageSite};

use crate::agentic::semantic::EvidenceKind;
use serde::{Deserialize, Serialize};

/// One navigation hit with provenance (#1387 Phase 2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavHit {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub kind: EvidenceKind,
    pub snippet: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl NavHit {
    #[must_use]
    pub fn loc_key(&self) -> String {
        format!("{}:{}-{}", self.path, self.start_line, self.end_line)
    }
}

/// Shared honesty envelope for structural + lexical navigation (#1387).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NavResult {
    pub hits: Vec<NavHit>,
    pub rejected: Vec<(NavHit, String)>,
    pub candidates: usize,
    /// `false` when gather cuts, weak heuristics, or missing indexes apply.
    pub complete: bool,
    pub index_id: String,
    pub warnings: Vec<String>,
    /// Analyzer identity (`regex-floor`, `where_is`, `lexical`, …).
    pub analyzer: String,
    pub kind: EvidenceKind,
    /// How many hits precede `hits[0]` — nonzero only on a later page (see
    /// [`NavResult::paged`]), so ordinals continue across pages.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ordinal_offset: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl NavResult {
    #[must_use]
    pub fn empty(kind: EvidenceKind, analyzer: &str, index_id: impl Into<String>) -> Self {
        Self {
            hits: Vec::new(),
            rejected: Vec::new(),
            candidates: 0,
            complete: true,
            index_id: index_id.into(),
            warnings: Vec::new(),
            analyzer: analyzer.to_string(),
            kind,
            ordinal_offset: 0,
        }
    }

    /// Keep one `page_size` window of `hits` (1-based `page`, default 1) and
    /// say where it sits — `showing hits 26-50 of 60; pass page: 3 …`.
    ///
    /// A result that fits one page is returned untouched. Paging exists so a
    /// long hit list never overflows into a content spill: a spill hands the
    /// model a `spill:<cid>` handle and a second tool to learn, where "ask for
    /// page 2" is an affordance every model already has. `candidates` keeps
    /// the full count, so the envelope never under-reports what was found.
    #[must_use]
    pub fn paged(mut self, page: Option<usize>, page_size: usize) -> Self {
        let total = self.hits.len();
        let page = page.filter(|&p| p > 0).unwrap_or(1);
        if page_size == 0 || (page == 1 && total <= page_size) {
            return self;
        }
        let pages = total.div_ceil(page_size);
        let start = (page - 1) * page_size;
        if start >= total {
            self.hits.clear();
            self.warnings.push(format!(
                "page {page} is past the end — {total} hits in {pages} pages of {page_size}"
            ));
            return self;
        }
        let end = (start + page_size).min(total);
        self.hits = self.hits.drain(start..end).collect();
        self.ordinal_offset = start;
        let mut note = format!("showing hits {}-{end} of {total}", start + 1);
        if end < total {
            note.push_str(&format!(
                "; pass `page: {}` for the next {}",
                page + 1,
                (total - end).min(page_size)
            ));
        } else {
            note.push_str(" (last page)");
        }
        self.warnings.push(note);
        self
    }

    /// Human/model text view — always includes kind labels + honesty footer.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{} nav — analyzer={}  complete={}  index={}  candidates={} hits={}\n",
            self.kind.label(),
            self.analyzer,
            self.complete,
            self.index_id,
            self.candidates,
            self.hits.len(),
        ));
        for (i, hit) in self.hits.iter().enumerate() {
            out.push_str(&format!(
                "  {:>2}. {} {}:{}-{}",
                self.ordinal_offset + i + 1,
                hit.kind.label(),
                hit.path,
                hit.start_line,
                hit.end_line,
            ));
            if let Some(sym) = &hit.symbol {
                out.push_str(&format!("  `{sym}`"));
            }
            if let Some(d) = &hit.detail {
                out.push_str(&format!("  ({d})"));
            }
            out.push('\n');
            if !hit.snippet.is_empty() {
                for line in hit.snippet.lines().take(8) {
                    out.push_str(&format!("      {line}\n"));
                }
            }
        }
        if self.hits.is_empty() {
            out.push_str("  (no hits)\n");
        }
        for w in &self.warnings {
            out.push_str(&format!("  warning: {w}\n"));
        }
        if !self.complete {
            out.push_str("  NOTE: incomplete — do not treat misses as proof of absence.\n");
        }
        out
    }

    /// Model-facing compact packet (same honesty fields).
    #[must_use]
    pub fn render_model(&self) -> String {
        let mut body = format!(
            "<nav_result kind=\"{}\" analyzer=\"{}\" complete=\"{}\" index_id=\"{}\">\n",
            match self.kind {
                EvidenceKind::Lexical => "lexical",
                EvidenceKind::Symbol => "symbol",
                EvidenceKind::Graph => "graph",
                EvidenceKind::Semantic => "semantic",
                EvidenceKind::Curated => "curated",
            },
            self.analyzer,
            self.complete,
            self.index_id,
        );
        body.push_str(&format!(
            "// {} evidence via {} — not a typechecker / LSP proof.\n",
            self.kind.label(),
            self.analyzer
        ));
        for hit in &self.hits {
            body.push_str(&format!(
                "// {} {}:{}-{}\n{}\n\n",
                hit.kind.label(),
                hit.path,
                hit.start_line,
                hit.end_line,
                hit.snippet
            ));
        }
        for w in &self.warnings {
            body.push_str(&format!("// warning: {w}\n"));
        }
        body.push_str("</nav_result>");
        body
    }
}

/// Session-scoped navigator state (#1387). Cleared on `/new` with the semantic
/// index (except as noted for individual fields).
#[derive(Debug, Default)]
pub struct NavigatorSession {
    pub usage: Option<UsageIndex>,
    pub graph: Option<GraphIndex>,
    pub project: Option<crate::project_model::ProjectModel>,
    pub files: Vec<(String, String)>,
    pub ledger: RetrievalLedger,
    pub last_nav: Option<NavResult>,
    pub last_semantic: Option<crate::agentic::semantic::RetrievalResult>,
    pub last_lexical: Option<NavResult>,
    pub map_expand: Option<String>,
    pub turn_counter: u64,
}

impl NavigatorSession {
    pub fn clear(&mut self) {
        self.usage = None;
        self.graph = None;
        self.project = None;
        self.files.clear();
        self.ledger.clear();
        self.last_nav = None;
        self.last_semantic = None;
        self.last_lexical = None;
        self.map_expand = None;
        self.turn_counter = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lexical_hits(n: usize) -> NavResult {
        let mut r = NavResult::empty(EvidenceKind::Lexical, "lexical-regex", "gen0");
        for i in 1..=n {
            r.hits.push(NavHit {
                path: "src/lib.rs".into(),
                start_line: i,
                end_line: i,
                kind: EvidenceKind::Lexical,
                snippet: format!("hit {i}"),
                symbol: None,
                detail: None,
            });
        }
        r.candidates = n;
        r
    }

    /// A big hit list is paged, not spilled: a spill hands a weak model a
    /// `spill:<cid>` handle it then misuses (live 2026-09-23), while "page 2"
    /// is an affordance every model already knows.
    #[test]
    fn paging_keeps_one_window_and_names_the_next_page() {
        let r = lexical_hits(60).paged(Some(2), 25);
        assert_eq!(r.hits.len(), 25);
        assert_eq!(r.hits[0].start_line, 26);
        assert_eq!(r.candidates, 60, "the total stays honest");
        let text = r.render();
        assert!(text.contains("26-50 of 60"), "{text}");
        assert!(text.contains("page: 3"), "{text}");
        // Ordinals continue across pages, so hit 26 is numbered 26.
        assert!(text.contains("26. [LEXICAL]"), "{text}");
    }

    #[test]
    fn the_last_page_says_it_is_the_last() {
        let text = lexical_hits(60).paged(Some(3), 25).render();
        assert!(text.contains("51-60 of 60"), "{text}");
        assert!(!text.contains("page: 4"), "{text}");
    }

    #[test]
    fn a_page_past_the_end_says_how_many_pages_exist() {
        let r = lexical_hits(60).paged(Some(9), 25);
        assert!(r.hits.is_empty());
        assert!(r.render().contains("3 pages"), "{}", r.render());
    }

    #[test]
    fn a_result_that_fits_one_page_is_untouched() {
        let before = lexical_hits(10);
        let after = before.clone().paged(None, 25);
        assert_eq!(before.render(), after.render());
    }

    #[test]
    fn nav_result_render_includes_kind_and_incomplete() {
        let mut r = NavResult::empty(EvidenceKind::Graph, "regex-floor", "gen1");
        r.complete = false;
        r.warnings.push("weak call-name match".into());
        r.hits.push(NavHit {
            path: "a.rs".into(),
            start_line: 3,
            end_line: 3,
            kind: EvidenceKind::Graph,
            snippet: "foo();".into(),
            symbol: Some("foo".into()),
            detail: Some("caller".into()),
        });
        let text = r.render();
        assert!(text.contains("[GRAPH]"));
        assert!(text.contains("complete=false"));
        assert!(text.contains("incomplete"));
        assert!(text.contains("a.rs:3-3"));
    }
}
