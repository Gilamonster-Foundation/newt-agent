//! `where_is` (#1285, spec §3.2 / SC-L5) — the exact, typed-verdict symbol
//! resolver: the anti-confabulation counterpart to `code_search`'s by-meaning
//! recall. Over the retained structural index (the SAME `(path, symbol, kind)`
//! facts the API surface renders), a lookup returns exactly one of three
//! verdicts under the **conservative rule**:
//!
//! - [`Found`](LookupVerdict::Found) — witnessed in the index (every witness
//!   returned);
//! - [`NotGathered`](LookupVerdict::NotGathered) — a miss while the snapshot has
//!   **open cuts**: lists the open cut *classes* as possibilities, never a
//!   per-symbol attribution (the index knows cut paths and classes, never the
//!   symbols inside a region it never extracted);
//! - [`NoSuchSymbol`](LookupVerdict::NoSuchSymbol) — a miss on a **cut-free**
//!   snapshot (complete gather): the only state in which "it does not exist" is
//!   assertable (SC-L5).
//!
//! A hit is never a ranked guess (contrast `code_search`); a miss is a verdict,
//! never a guess. [`WhereIsIndex::where_is`] is pure + total (SC-PO-3), so it is
//! Aeneas-extractable alongside `render`.
//!
//! This is distinct from the verify-gate [`crate::symbols::Verdict`]
//! (`Resolved | NotBuilt | Fabricated`, module-keyed fabrication detection) —
//! same lineage (SC-L5 reformulated), different trust concern.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::agentic::semantic::{CutClass, GatherManifest};
use crate::config::LanguagePack;

/// One place a symbol is witnessed: the file it was extracted from and the
/// free-form kind label the language pack assigned (`fn`, `struct`, …).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Witness {
    pub path: String,
    pub kind: String,
}

/// The lookup verdict (spec §3.2) — `Found | NotGathered | NoSuchSymbol` under
/// the conservative rule. JSON wire tag matches the spec (`snake_case`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum LookupVerdict {
    /// Witnessed in the retained index; every witness is returned.
    Found {
        /// The `(path, kind)` sites, deduped + sorted (order-independent).
        witnesses: Vec<Witness>,
    },
    /// A miss while the snapshot has open cuts. `cuts_open` lists the open cut
    /// classes as **possibilities** — never a per-symbol attribution.
    NotGathered {
        /// The distinct open cut classes (e.g. `too_large`, `over_file_cap`).
        cuts_open: Vec<String>,
    },
    /// A miss on a cut-free (complete) gather: the symbol does not exist.
    NoSuchSymbol,
}

/// The retained `where_is` index: a symbol→witnesses table plus the snapshot's
/// open cut classes (empty ⇒ a cut-free / complete gather). Built once per
/// snapshot; every lookup is a pure function of this value (SC-L5 §5).
#[derive(Debug, Clone, Default)]
pub struct WhereIsIndex {
    by_symbol: BTreeMap<String, Vec<Witness>>,
    cuts_open: Vec<String>,
}

impl WhereIsIndex {
    /// Build from extracted `(symbol, path, kind)` facts and the snapshot's open
    /// cut classes. Witnesses are deduped + sorted so the verdict is a pure
    /// function of the fact *set* (input order cannot change an answer).
    pub fn from_facts(
        facts: impl IntoIterator<Item = (String, String, String)>,
        cuts_open: Vec<String>,
    ) -> Self {
        let mut by_symbol: BTreeMap<String, Vec<Witness>> = BTreeMap::new();
        for (symbol, path, kind) in facts {
            by_symbol
                .entry(symbol)
                .or_default()
                .push(Witness { path, kind });
        }
        for witnesses in by_symbol.values_mut() {
            witnesses.sort();
            witnesses.dedup();
        }
        let mut cuts_open = cuts_open;
        cuts_open.sort();
        cuts_open.dedup();
        Self {
            by_symbol,
            cuts_open,
        }
    }

    /// True on a complete gather (no open cuts) — the only state in which
    /// [`NoSuchSymbol`](LookupVerdict::NoSuchSymbol) is assertable (SC-L5).
    #[must_use]
    pub fn cut_free(&self) -> bool {
        self.cuts_open.is_empty()
    }

    /// Number of distinct symbols witnessed — the tool's honesty line.
    #[must_use]
    pub fn symbol_count(&self) -> usize {
        self.by_symbol.len()
    }

    /// The conservative-rule resolver (SC-L5 / SC-PO-3): **total** — exactly one
    /// verdict for every input. `kind` (optional) filters witnesses by the pack
    /// label; a kind that filters *all* witnesses away is still a miss.
    #[must_use]
    pub fn where_is(&self, symbol: &str, kind: Option<&str>) -> LookupVerdict {
        let witnesses: Vec<Witness> = self
            .by_symbol
            .get(symbol)
            .into_iter()
            .flatten()
            .filter(|w| match kind {
                Some(k) => w.kind == k,
                None => true,
            })
            .cloned()
            .collect();
        if !witnesses.is_empty() {
            LookupVerdict::Found { witnesses }
        } else if self.cut_free() {
            LookupVerdict::NoSuchSymbol
        } else {
            LookupVerdict::NotGathered {
                cuts_open: self.cuts_open.clone(),
            }
        }
    }
}

/// The snake_case cut-class label used in a `NotGathered` verdict's `cuts_open`.
#[must_use]
fn cut_class_label(class: CutClass) -> &'static str {
    match class {
        CutClass::TooLarge => "too_large",
        CutClass::OverFileCap => "over_file_cap",
    }
}

/// The distinct open cut classes of a gather — empty ⇒ cut-free (SC-L5).
#[must_use]
fn cuts_open_from_manifest(manifest: &GatherManifest) -> Vec<String> {
    manifest
        .cuts
        .iter()
        .map(|c| cut_class_label(c.class).to_string())
        .collect()
}

/// Build a `where_is` index from already-gathered files, using the language
/// packs' symbol extraction (the SAME facts the API surface renders, via
/// [`crate::api_surface::symbol_facts`]) and the gather manifest's cuts (so the
/// conservative rule sees the real open-cut set). Pure (no filesystem) — the
/// caller does the honest gather, this derives the retained index from it.
#[must_use]
pub fn build_where_is_index(
    files: &[(String, String)],
    packs: &[LanguagePack],
    manifest: &GatherManifest,
) -> WhereIsIndex {
    let facts = crate::api_surface::symbol_facts(packs, files);
    WhereIsIndex::from_facts(facts, cuts_open_from_manifest(manifest))
}

/// The live-loop entry point (#1285): honest-gather the workspace's code files
/// over the built-in packs' extensions and build the index. A thin filesystem
/// wrapper over the pure [`build_where_is_index`] — **model-free**, so the
/// harness builds it every session regardless of embeddings. Reset it on `/new`
/// the same way the semantic index resets.
#[must_use]
pub fn build_where_is_index_from_workspace(workspace: &str) -> WhereIsIndex {
    use crate::agentic::semantic::{gather_with_manifest, GatherCaps};
    let packs = crate::api_surface::builtin_packs();
    let exts: Vec<String> = packs
        .iter()
        .flat_map(|p| p.extensions.iter().cloned())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let (files, manifest) = gather_with_manifest(workspace, &exts, GatherCaps::default());
    build_where_is_index(&files, &packs, &manifest)
}

/// The `where_is` tool definition (advertised only when an index is present).
/// Self-teaching (gilabot-style) so a small model prefers one classified lookup
/// to N grep rounds.
#[must_use]
pub fn where_is_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "where_is",
            "description": "Locate EXACTLY where a symbol (function, struct, class, …) is defined, by NAME — a typed verdict, never a ranked guess. Returns one of: FOUND (the file(s) that define it), NOT_GATHERED (it wasn't in the indexed files, plus which file classes were skipped — so it MIGHT live in a skipped file), or NO_SUCH_SYMBOL (a complete index has no such name — it does not exist here). Prefer ONE where_is over many grep/read rounds. Use `code_search` instead when you only know the MEANING, not the name.",
            "parameters": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "The exact symbol name to locate, e.g. `run_crew`." },
                    "kind": { "type": "string", "description": "Optional kind filter — the language-pack label, e.g. `fn`, `struct`, `class`." }
                },
                "required": ["symbol"]
            }
        }
    })
}

/// Execute a `where_is` call: parse `symbol` (+ optional `kind`), resolve, and
/// render the verdict as self-explaining text for the model. `NotGathered` is
/// explicitly framed as "may exist in a skipped file", never a denial (SC-L5).
#[must_use]
pub fn execute_where_is(
    args: &serde_json::Value,
    index: &WhereIsIndex,
    tool_output_lines: usize,
) -> String {
    let Some(symbol) = args
        .get("symbol")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return "error: where_is needs a non-empty `symbol` string".to_string();
    };
    let kind = args
        .get("kind")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    match index.where_is(symbol, kind) {
        LookupVerdict::Found { witnesses } => {
            let n = witnesses.len();
            let mut out = format!(
                "where_is `{symbol}`: FOUND ({n} witness{})\n",
                if n == 1 { "" } else { "es" }
            );
            let cap = tool_output_lines.max(1);
            for w in witnesses.iter().take(cap) {
                out.push_str(&format!("  {} ({})\n", w.path, w.kind));
            }
            if n > cap {
                out.push_str(&format!("  … and {} more witness(es)\n", n - cap));
            }
            out
        }
        LookupVerdict::NotGathered { cuts_open } => format!(
            "where_is `{symbol}`: NOT_GATHERED — not in the indexed files, but the gather \
             skipped some (open cut classes: [{}]). It may live in a skipped file — read_file \
             or code_search to confirm. This is NOT a claim that it doesn't exist.",
            cuts_open.join(", ")
        ),
        LookupVerdict::NoSuchSymbol => format!(
            "where_is `{symbol}`: NO_SUCH_SYMBOL — the index is complete (no cuts) and has no \
             symbol by that name. It is not defined in this workspace."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentic::semantic::{Cut, GatherManifest};

    fn facts(pairs: &[(&str, &str, &str)]) -> Vec<(String, String, String)> {
        pairs
            .iter()
            .map(|(s, p, k)| (s.to_string(), p.to_string(), k.to_string()))
            .collect()
    }

    fn manifest(cuts: Vec<Cut>) -> GatherManifest {
        GatherManifest {
            candidate_count: 0,
            candidate_hash: String::new(),
            max_files: 400,
            max_bytes: 200_000,
            cuts,
        }
    }

    // ---- SC-PO-3: Found ↔ ∃ witness ; witnesses ⊆ entries ----

    #[test]
    fn found_returns_every_witness() {
        let idx = WhereIsIndex::from_facts(
            facts(&[
                ("run_crew", "crew.rs", "fn"),
                ("run_crew", "crew_tool.rs", "fn"),
            ]),
            vec![],
        );
        match idx.where_is("run_crew", None) {
            LookupVerdict::Found { witnesses } => {
                assert_eq!(witnesses.len(), 2);
                assert!(witnesses.iter().any(|w| w.path == "crew.rs"));
                assert!(witnesses.iter().any(|w| w.path == "crew_tool.rs"));
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn kind_filters_witnesses_and_a_full_filter_is_a_miss() {
        let idx = WhereIsIndex::from_facts(
            facts(&[
                ("Router", "router.rs", "struct"),
                ("Router", "mod.rs", "fn"),
            ]),
            vec![], // cut-free
        );
        // kind=struct → only the struct witness
        match idx.where_is("Router", Some("struct")) {
            LookupVerdict::Found { witnesses } => {
                assert_eq!(witnesses.len(), 1);
                assert_eq!(witnesses[0].kind, "struct");
            }
            other => panic!("expected Found, got {other:?}"),
        }
        // kind=enum → filters all → miss → NoSuchSymbol (cut-free)
        assert_eq!(
            idx.where_is("Router", Some("enum")),
            LookupVerdict::NoSuchSymbol
        );
    }

    // ---- SC-L5: the conservative rule ----

    #[test]
    fn no_such_symbol_only_on_a_cut_free_snapshot() {
        let idx = WhereIsIndex::from_facts(facts(&[("a", "a.rs", "fn")]), vec![]);
        assert!(idx.cut_free());
        assert_eq!(idx.where_is("nope", None), LookupVerdict::NoSuchSymbol);
    }

    #[test]
    fn a_miss_with_open_cuts_is_not_gathered_never_no_such_symbol() {
        let idx = WhereIsIndex::from_facts(
            facts(&[("a", "a.rs", "fn")]),
            vec!["too_large".into(), "over_file_cap".into()],
        );
        assert!(!idx.cut_free());
        match idx.where_is("nope", None) {
            LookupVerdict::NotGathered { cuts_open } => {
                assert_eq!(cuts_open, vec!["over_file_cap", "too_large"]); // sorted, deduped
            }
            other => panic!("expected NotGathered, got {other:?}"),
        }
    }

    #[test]
    fn cuts_open_is_deduped_and_sorted() {
        let idx = WhereIsIndex::from_facts(
            facts(&[]),
            vec![
                "too_large".into(),
                "too_large".into(),
                "over_file_cap".into(),
            ],
        );
        match idx.where_is("x", None) {
            LookupVerdict::NotGathered { cuts_open } => {
                assert_eq!(cuts_open, vec!["over_file_cap", "too_large"]);
            }
            other => panic!("expected NotGathered, got {other:?}"),
        }
    }

    #[test]
    fn found_wins_even_when_cuts_are_open() {
        // A hit is a hit regardless of open cuts — cuts only govern the miss arm.
        let idx = WhereIsIndex::from_facts(facts(&[("a", "a.rs", "fn")]), vec!["too_large".into()]);
        assert!(matches!(
            idx.where_is("a", None),
            LookupVerdict::Found { .. }
        ));
    }

    #[test]
    fn resolver_is_total_exactly_one_verdict_per_input() {
        let idx = WhereIsIndex::from_facts(facts(&[("a", "a.rs", "fn")]), vec![]);
        for sym in ["a", "b", "", "A", "run_crew"] {
            // where_is never panics and always returns a verdict.
            let _ = idx.where_is(sym, None);
            let _ = idx.where_is(sym, Some("fn"));
        }
    }

    #[test]
    fn witnesses_are_deduped_and_order_independent() {
        let a = WhereIsIndex::from_facts(
            facts(&[
                ("s", "b.rs", "fn"),
                ("s", "a.rs", "fn"),
                ("s", "a.rs", "fn"),
            ]),
            vec![],
        );
        let b = WhereIsIndex::from_facts(
            facts(&[
                ("s", "a.rs", "fn"),
                ("s", "a.rs", "fn"),
                ("s", "b.rs", "fn"),
            ]),
            vec![],
        );
        assert_eq!(a.where_is("s", None), b.where_is("s", None));
        match a.where_is("s", None) {
            LookupVerdict::Found { witnesses } => assert_eq!(witnesses.len(), 2), // deduped
            other => panic!("{other:?}"),
        }
    }

    // ---- builder: extracts the same facts the surface renders ----

    #[test]
    fn build_index_extracts_rust_symbols_from_gathered_files() {
        let files = vec![(
            "src/router.rs".to_string(),
            "pub struct Router {}\npub fn boot() {}".to_string(),
        )];
        let idx = build_where_is_index(
            &files,
            &crate::api_surface::builtin_packs(),
            &manifest(vec![]),
        );
        assert!(matches!(
            idx.where_is("Router", None),
            LookupVerdict::Found { .. }
        ));
        assert!(matches!(
            idx.where_is("boot", None),
            LookupVerdict::Found { .. }
        ));
        // A complete gather (no cuts) → an absent name is NoSuchSymbol.
        assert_eq!(idx.where_is("missing", None), LookupVerdict::NoSuchSymbol);
    }

    #[test]
    fn build_index_carries_the_manifest_cuts_into_the_conservative_rule() {
        let files = vec![("a.rs".to_string(), "pub fn a() {}".to_string())];
        let idx = build_where_is_index(
            &files,
            &crate::api_surface::builtin_packs(),
            &manifest(vec![Cut {
                path: "big.rs".into(),
                class: CutClass::TooLarge,
            }]),
        );
        // A miss is NOT_GATHERED because the manifest carried an open cut.
        match idx.where_is("missing", None) {
            LookupVerdict::NotGathered { cuts_open } => {
                assert_eq!(cuts_open, vec!["too_large"]);
            }
            other => panic!("expected NotGathered, got {other:?}"),
        }
    }

    // ---- the tool: verdict rendering ----

    #[test]
    fn tool_definition_is_named_where_is_with_required_symbol() {
        let def = where_is_tool_definition();
        assert_eq!(def["function"]["name"], "where_is");
        assert_eq!(def["function"]["parameters"]["required"][0], "symbol");
    }

    #[test]
    fn execute_renders_each_verdict_and_coaches_on_empty_input() {
        let idx = WhereIsIndex::from_facts(
            facts(&[("boot", "router.rs", "fn")]),
            vec!["too_large".into()],
        );
        let found = execute_where_is(&serde_json::json!({"symbol": "boot"}), &idx, 20);
        assert!(found.contains("FOUND") && found.contains("router.rs"));

        let miss = execute_where_is(&serde_json::json!({"symbol": "nope"}), &idx, 20);
        assert!(miss.contains("NOT_GATHERED") && miss.contains("too_large"));

        // empty symbol → coaching, not a lookup
        let empty = execute_where_is(&serde_json::json!({}), &idx, 20);
        assert!(empty.starts_with("error:"));
    }

    #[test]
    fn execute_says_no_such_symbol_only_on_a_complete_index() {
        let idx = WhereIsIndex::from_facts(facts(&[("a", "a.rs", "fn")]), vec![]);
        let out = execute_where_is(&serde_json::json!({"symbol": "ghost"}), &idx, 20);
        assert!(out.contains("NO_SUCH_SYMBOL"));
    }
}
