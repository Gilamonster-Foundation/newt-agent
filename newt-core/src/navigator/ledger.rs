//! Per-turn [`RetrievalLedger`] — context debugging (#1387 Phase 3).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::agentic::semantic::{EvidenceKind, RankedHit, RejectReason, RetrievalResult};

use super::NavResult;

/// One turn's retrieval / nav audit trail.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TurnRetrieval {
    pub turn: u64,
    pub query: String,
    pub selected: Vec<LedgerHit>,
    pub rejected: Vec<LedgerReject>,
    pub pins: Vec<String>,
    pub excludes: Vec<String>,
    pub nav_summary: Vec<String>,
    pub context_hash: String,
    pub index_id: String,
    pub complete: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LedgerHit {
    pub loc: String,
    pub kind: EvidenceKind,
    pub score: Option<f32>,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LedgerReject {
    pub loc: String,
    pub reason: String,
    pub kind: EvidenceKind,
}

/// Session ledger of per-turn retrieval.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RetrievalLedger {
    pub turns: Vec<TurnRetrieval>,
    /// Previous session-index id (generation) for compare previous/current.
    pub previous_index_id: Option<String>,
    pub current_index_id: Option<String>,
}

impl RetrievalLedger {
    pub fn clear(&mut self) {
        self.turns.clear();
        self.previous_index_id = self.current_index_id.take();
        self.current_index_id = None;
    }

    pub fn set_index(&mut self, index_id: impl Into<String>) {
        let id = index_id.into();
        if self.current_index_id.as_ref() != Some(&id) {
            if let Some(cur) = self.current_index_id.take() {
                self.previous_index_id = Some(cur);
            }
            self.current_index_id = Some(id);
        }
    }

    pub fn record_semantic(
        &mut self,
        turn: u64,
        query: &str,
        result: &RetrievalResult,
        pins: &[String],
        excludes: &[String],
        context_hash: &str,
    ) {
        let selected = result
            .hits
            .iter()
            .map(|h| LedgerHit {
                loc: h.loc_key(),
                kind: h.kind,
                score: Some(h.final_score),
                path: h.chunk.file.clone(),
            })
            .collect();
        let rejected = result
            .rejected
            .iter()
            .map(|(h, r)| LedgerReject {
                loc: h.loc_key(),
                reason: reject_label(*r).into(),
                kind: h.kind,
            })
            .collect();
        self.turns.push(TurnRetrieval {
            turn,
            query: query.to_string(),
            selected,
            rejected,
            pins: pins.to_vec(),
            excludes: excludes.to_vec(),
            nav_summary: Vec::new(),
            context_hash: context_hash.to_string(),
            index_id: result.index_id.clone(),
            complete: result.complete,
            warnings: result.warnings.clone(),
        });
        self.set_index(&result.index_id);
    }

    pub fn record_nav(&mut self, turn: u64, query: &str, nav: &NavResult, context_hash: &str) {
        let selected = nav
            .hits
            .iter()
            .map(|h| LedgerHit {
                loc: h.loc_key(),
                kind: h.kind,
                score: None,
                path: h.path.clone(),
            })
            .collect();
        let rejected = nav
            .rejected
            .iter()
            .map(|(h, r)| LedgerReject {
                loc: h.loc_key(),
                reason: r.clone(),
                kind: h.kind,
            })
            .collect();
        self.turns.push(TurnRetrieval {
            turn,
            query: query.to_string(),
            selected,
            rejected,
            pins: Vec::new(),
            excludes: Vec::new(),
            nav_summary: vec![format!(
                "{} {} hits complete={}",
                nav.kind.label(),
                nav.hits.len(),
                nav.complete
            )],
            context_hash: context_hash.to_string(),
            index_id: nav.index_id.clone(),
            complete: nav.complete,
            warnings: nav.warnings.clone(),
        });
        self.set_index(&nav.index_id);
    }

    #[must_use]
    pub fn get_turn(&self, n: u64) -> Option<&TurnRetrieval> {
        self.turns.iter().find(|t| t.turn == n).or_else(|| {
            // Also allow 1-based index into the vec.
            let idx = n.saturating_sub(1) as usize;
            self.turns.get(idx)
        })
    }

    /// Prior entry for `/retrieval turn N diff`: prefer turn `N-1`, else the
    /// previous ledger row before `N`.
    #[must_use]
    pub fn prior_turn(&self, turn: u64) -> Option<&TurnRetrieval> {
        if turn > 0 {
            if let Some(t) = self.turns.iter().find(|t| t.turn == turn.saturating_sub(1)) {
                return Some(t);
            }
        }
        let idx = self.turns.iter().position(|t| t.turn == turn)?;
        idx.checked_sub(1).and_then(|i| self.turns.get(i))
    }
}

/// Blake3 hex over inject-block / model-packet bytes (ledger `context_hash`).
#[must_use]
pub fn hash_context(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn reject_label(r: RejectReason) -> &'static str {
    match r {
        RejectReason::BelowTopK => "below_top_k",
        RejectReason::BudgetExhausted => "budget_exhausted",
        RejectReason::Excluded => "excluded",
    }
}

#[must_use]
pub fn format_ledger_human(t: &TurnRetrieval) -> String {
    let mut out = format!(
        "retrieval turn {} — query: {:?}\n  index={} complete={} context_hash={}\n",
        t.turn, t.query, t.index_id, t.complete, t.context_hash
    );
    out.push_str(&format!("  selected ({}):\n", t.selected.len()));
    for h in &t.selected {
        out.push_str(&format!(
            "    {} {} {}\n",
            h.kind.label(),
            h.loc,
            h.score.map(|s| format!("score={s:.3}")).unwrap_or_default()
        ));
    }
    out.push_str(&format!("  rejected ({}):\n", t.rejected.len()));
    for r in &t.rejected {
        out.push_str(&format!(
            "    {} {} ({})\n",
            r.kind.label(),
            r.loc,
            r.reason
        ));
    }
    if !t.pins.is_empty() {
        out.push_str(&format!("  pins: {}\n", t.pins.join(", ")));
    }
    if !t.excludes.is_empty() {
        out.push_str(&format!("  excludes: {}\n", t.excludes.join(", ")));
    }
    for s in &t.nav_summary {
        out.push_str(&format!("  nav: {s}\n"));
    }
    for w in &t.warnings {
        out.push_str(&format!("  warning: {w}\n"));
    }
    out
}

#[must_use]
pub fn format_ledger_model(t: &TurnRetrieval) -> String {
    serde_json::to_string_pretty(t).unwrap_or_else(|_| "{}".into())
}

#[must_use]
pub fn format_ledger_diff(a: &TurnRetrieval, b: &TurnRetrieval) -> String {
    let a_locs: BTreeMap<&str, &LedgerHit> =
        a.selected.iter().map(|h| (h.loc.as_str(), h)).collect();
    let b_locs: BTreeMap<&str, &LedgerHit> =
        b.selected.iter().map(|h| (h.loc.as_str(), h)).collect();
    let mut out = format!("compare turn {} ↔ turn {}\n  only in A:\n", a.turn, b.turn);
    for (loc, h) in &a_locs {
        if !b_locs.contains_key(loc) {
            out.push_str(&format!("    - {} {}\n", h.kind.label(), loc));
        }
    }
    out.push_str("  only in B:\n");
    for (loc, h) in &b_locs {
        if !a_locs.contains_key(loc) {
            out.push_str(&format!("    + {} {}\n", h.kind.label(), loc));
        }
    }
    out.push_str("  in both:\n");
    for (loc, h) in &a_locs {
        if b_locs.contains_key(loc) {
            out.push_str(&format!("    = {} {}\n", h.kind.label(), loc));
        }
    }
    out
}

#[must_use]
pub fn compare_ledgers(ledger: &RetrievalLedger, a: u64, b: u64) -> String {
    match (ledger.get_turn(a), ledger.get_turn(b)) {
        (Some(ta), Some(tb)) => format_ledger_diff(ta, tb),
        _ => format!(
            "missing turn(s): A={a} B={b} (have {} turns)\n",
            ledger.turns.len()
        ),
    }
}

/// Compare last semantic vs last lexical result sets by path.
#[must_use]
pub fn compare_semantic_lexical(
    semantic: Option<&RetrievalResult>,
    lexical: Option<&NavResult>,
) -> String {
    let mut out = String::from("compare semantic ↔ lexical\n");
    let sem_paths: BTreeMap<String, &RankedHit> = semantic
        .map(|r| r.hits.iter().map(|h| (h.chunk.file.clone(), h)).collect())
        .unwrap_or_default();
    let lex_paths: BTreeMap<String, _> = lexical
        .map(|r| r.hits.iter().map(|h| (h.path.clone(), h)).collect())
        .unwrap_or_default();
    out.push_str(&format!(
        "  semantic hits={}  lexical hits={}\n",
        sem_paths.len(),
        lex_paths.len()
    ));
    out.push_str("  only semantic:\n");
    for (p, h) in &sem_paths {
        if !lex_paths.contains_key(p) {
            out.push_str(&format!("    - {} {}\n", h.kind.label(), p));
        }
    }
    out.push_str("  only lexical:\n");
    for (p, h) in &lex_paths {
        if !sem_paths.contains_key(p) {
            out.push_str(&format!("    + {} {}\n", h.kind.label(), p));
        }
    }
    out.push_str("  both:\n");
    for p in sem_paths.keys() {
        if lex_paths.contains_key(p) {
            out.push_str(&format!("    = {p}\n"));
        }
    }
    out
}

#[must_use]
pub fn export_ledger_json(ledger: &RetrievalLedger) -> String {
    serde_json::to_string_pretty(ledger).unwrap_or_else(|_| "{}".into())
}

#[must_use]
pub fn export_ledger_markdown(ledger: &RetrievalLedger) -> String {
    let mut out = String::from("# Retrieval Ledger\n\n");
    if let Some(cur) = &ledger.current_index_id {
        out.push_str(&format!("- current index: `{cur}`\n"));
    }
    if let Some(prev) = &ledger.previous_index_id {
        out.push_str(&format!("- previous index: `{prev}`\n"));
    }
    out.push('\n');
    for t in &ledger.turns {
        out.push_str(&format!("## Turn {}\n\n", t.turn));
        out.push_str(&format!("- query: `{}`\n", t.query));
        out.push_str(&format!(
            "- index: `{}` complete={}\n",
            t.index_id, t.complete
        ));
        out.push_str(&format!("- context_hash: `{}`\n", t.context_hash));
        out.push_str(&format!("- selected: {}\n", t.selected.len()));
        for h in &t.selected {
            out.push_str(&format!("  - {} `{}`\n", h.kind.label(), h.loc));
        }
        out.push_str(&format!("- rejected: {}\n", t.rejected.len()));
        for r in &t.rejected {
            out.push_str(&format!(
                "  - {} `{}` ({})\n",
                r.kind.label(),
                r.loc,
                r.reason
            ));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentic::semantic::{CodeChunk, RankedHit, RetrievalResult};

    fn sample_result() -> RetrievalResult {
        RetrievalResult {
            hits: vec![RankedHit {
                chunk: CodeChunk {
                    file: "a.rs".into(),
                    start_line: 1,
                    end_line: 2,
                    kind: "function".into(),
                    text: "fn a() {}".into(),
                },
                kind: EvidenceKind::Semantic,
                cosine: 0.9,
                def_boost: 0.05,
                path_boost: 0.0,
                final_score: 0.95,
            }],
            rejected: vec![],
            candidates: 3,
            complete: true,
            index_id: "gen1:abcd".into(),
            warnings: vec!["results are SEMANTIC evidence".into()],
        }
    }

    #[test]
    fn golden_ledger_json_shape() {
        let mut ledger = RetrievalLedger::default();
        ledger.record_semantic(1, "retry backoff", &sample_result(), &[], &[], "ctxhash1");
        let json = export_ledger_json(&ledger);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["turns"][0]["turn"], 1);
        assert_eq!(v["turns"][0]["query"], "retry backoff");
        assert_eq!(v["turns"][0]["index_id"], "gen1:abcd");
        assert_eq!(v["turns"][0]["selected"][0]["loc"], "a.rs:1-2");
        assert_eq!(v["turns"][0]["selected"][0]["kind"], "semantic");
        assert_eq!(v["current_index_id"], "gen1:abcd");
        let md = export_ledger_markdown(&ledger);
        assert!(md.contains("# Retrieval Ledger"));
        assert!(md.contains("Turn 1"));
    }

    #[test]
    fn compare_turns() {
        let mut ledger = RetrievalLedger::default();
        let a = sample_result();
        let mut b = sample_result();
        b.hits[0].chunk.file = "b.rs".into();
        ledger.record_semantic(1, "q1", &a, &[], &[], "c1");
        ledger.record_semantic(2, "q2", &b, &[], &[], "c2");
        let diff = compare_ledgers(&ledger, 1, 2);
        assert!(diff.contains("only in A"));
        assert!(diff.contains("a.rs:1-2"));
        assert!(diff.contains("b.rs:1-2"));
    }

    #[test]
    fn prior_turn_prefers_n_minus_one() {
        let mut ledger = RetrievalLedger::default();
        ledger.record_semantic(1, "q1", &sample_result(), &[], &[], "c1");
        let mut mid = sample_result();
        mid.hits[0].chunk.file = "mid.rs".into();
        ledger.record_semantic(2, "q2", &mid, &[], &[], "c2");
        let mut late = sample_result();
        late.hits[0].chunk.file = "late.rs".into();
        ledger.record_semantic(5, "q5", &late, &[], &[], "c5");
        // No turn 4 — fall back to previous row (turn 2).
        let prior = ledger.prior_turn(5).unwrap();
        assert_eq!(prior.turn, 2);
        assert_eq!(prior.selected[0].path, "mid.rs");
        // Exact N-1 exists.
        assert_eq!(ledger.prior_turn(2).unwrap().turn, 1);
    }

    #[test]
    fn hash_context_is_stable_blake3_hex() {
        let a = hash_context(b"<code_evidence>...</code_evidence>");
        let b = hash_context(b"<code_evidence>...</code_evidence>");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert_ne!(a, hash_context(b"other"));
    }

    #[test]
    fn record_nav_curated_map_shape() {
        let mut ledger = RetrievalLedger::default();
        let mut nav = super::super::NavResult::empty(EvidenceKind::Curated, "project-map", "gen1");
        nav.hits.push(super::super::NavHit {
            path: "newt-core".into(),
            start_line: 1,
            end_line: 1,
            kind: EvidenceKind::Curated,
            snippet: "unit newt-core".into(),
            symbol: Some("newt-core".into()),
            detail: Some("unit".into()),
        });
        ledger.record_nav(1, "map", &nav, &hash_context(b"map"));
        assert_eq!(ledger.turns[0].selected[0].kind, EvidenceKind::Curated);
        assert!(ledger.turns[0].nav_summary[0].contains("[CURATED]"));
        assert!(ledger.turns[0].nav_summary[0].contains("1 hits"));
    }
}
