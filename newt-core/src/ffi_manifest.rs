//! FFI-introspection manifest — the **knowledge-base** support part (#74).
//!
//! The cross-family forensic (`docs/findings/2026-06-14-fabrication-is-sampling-
//! not-information-loss.md`) showed a model fabricates a PyO3 import surface —
//! `import newt_core` (the *crate* name) instead of the real
//! `newt_agent._newt_agent.core` — even though it *read* the binding source that
//! declares the path. The fix is not to make it re-read: it is to hand the model
//! the authoritative `{crate → import_path}` as a compact **structured fact** that
//! survives compression where 20k of verbatim Rust cannot.
//!
//! This module extracts that fact from the PyO3 binding sources. Each crate's
//! `src/pyo3_module.rs` declares its Python path on every `#[pyclass(module =
//! "…")]`, its exported classes via that `name = "…"`, and any non-class exports
//! via `m.add("…", …)`. We harvest those into an [`FfiManifest`] that can be:
//! - rendered as an injectable [`render_block`](FfiManifest::render_block) for the
//!   model's working set (the knowledge-base part), and
//! - turned into the authoritative [`SymbolIndex`](FfiManifest::to_symbol_index)
//!   the verify oracle resolves against (replacing the hand-maintained
//!   `python_surface.json` map in `pack_pyo3_corpus.sh`).
//!
//! Regex-floor, like [`crate::symbols`]: a `#[pyclass(module = …)]` declaration on
//! a normal attribute is in scope; macro-generated or `cfg`-hidden bindings are
//! not, until a syn/tree-sitter upgrade.

use crate::symbols::SymbolIndex;
use regex::Regex;
use std::path::Path;

/// One PyO3-bound crate's Python import surface, extracted from its binding source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfiModule {
    /// The source label — the Rust crate name (e.g. `"newt-core"`).
    pub source: String,
    /// The authoritative Python import path (e.g. `"newt_agent._newt_agent.core"`),
    /// taken from the `#[pyclass(module = "…")]` attribute.
    pub import_path: String,
    /// Exported Python symbol names (pyclass `name = "…"` + `m.add("…", …)`),
    /// sorted and de-duplicated for a deterministic surface.
    pub symbols: Vec<String>,
}

/// The extracted import surface of a set of PyO3-bound crates — the
/// knowledge-base fact handed to a model and the ground truth scored against.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FfiManifest {
    /// One entry per bound crate, in input order.
    pub modules: Vec<FfiModule>,
}

impl FfiModule {
    /// Extract a module from one PyO3 binding source. Returns `None` if the source
    /// declares no `#[pyclass(module = "…")]` — i.e. it is not a bound surface we
    /// can name authoritatively.
    #[must_use]
    pub fn from_source(source_label: &str, src: &str) -> Option<Self> {
        // `#[pyclass( … )]` attribute bodies, single- or multi-line.
        let pyclass_re = Regex::new(r"(?s)#\[pyclass\((.*?)\)\]").unwrap();
        let module_re = Regex::new(r#"\bmodule\s*=\s*"([^"]+)""#).unwrap();
        let name_re = Regex::new(r#"\bname\s*=\s*"([^"]+)""#).unwrap();
        // `m.add("Name", …)` — non-class exports (exceptions, constants).
        let add_re = Regex::new(r#"\.add\(\s*"([^"]+)""#).unwrap();

        let mut import_path: Option<String> = None;
        let mut symbols: Vec<String> = Vec::new();

        for caps in pyclass_re.captures_iter(src) {
            let body = &caps[1];
            if let Some(m) = module_re.captures(body) {
                // All pyclasses in a crate declare the same path; first wins.
                import_path.get_or_insert_with(|| m[1].to_string());
            }
            if let Some(n) = name_re.captures(body) {
                symbols.push(n[1].to_string());
            }
        }

        let import_path = import_path?;
        for caps in add_re.captures_iter(src) {
            symbols.push(caps[1].to_string());
        }
        symbols.sort();
        symbols.dedup();

        Some(Self {
            source: source_label.to_string(),
            import_path,
            symbols,
        })
    }
}

impl FfiManifest {
    /// Build a manifest from labelled binding sources `(crate_name, source)`.
    /// Sources with no bound surface are skipped.
    #[must_use]
    pub fn from_sources<'a>(sources: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let modules = sources
            .into_iter()
            .filter_map(|(label, src)| FfiModule::from_source(label, src))
            .collect();
        Self { modules }
    }

    /// Build a manifest by discovering `<crate>/src/pyo3_module.rs` under a
    /// workspace root — each top-level subdirectory is a crate, named by its
    /// directory. Crates without a readable binding are skipped; the result is
    /// deterministic (crates in sorted directory order).
    pub fn from_workspace(root: &Path) -> std::io::Result<Self> {
        let mut found: Vec<(String, String)> = Vec::new();
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let binding = entry.path().join("src").join("pyo3_module.rs");
            if let Ok(src) = std::fs::read_to_string(&binding) {
                found.push((entry.file_name().to_string_lossy().into_owned(), src));
            }
        }
        found.sort();
        Ok(Self::from_sources(
            found.iter().map(|(l, s)| (l.as_str(), s.as_str())),
        ))
    }

    /// Number of bound crates in the manifest.
    #[must_use]
    pub fn len(&self) -> usize {
        self.modules.len()
    }

    /// Whether the manifest is empty (no bound surface extracted).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }

    /// Render the manifest as an injectable, model-facing knowledge-base block —
    /// the authoritative import paths as a compact structured fact. Empty manifest
    /// renders to an empty string (inject nothing).
    #[must_use]
    pub fn render_block(&self) -> String {
        if self.modules.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "[PYO3 IMPORT SURFACE — authoritative; import these exact paths, \
             do not infer a path from the crate name]\n",
        );
        for m in &self.modules {
            out.push_str("- ");
            out.push_str(&m.source);
            out.push_str(" → ");
            out.push_str(&m.import_path);
            if !m.symbols.is_empty() {
                out.push_str("  (");
                out.push_str(&m.symbols.join(", "));
                out.push(')');
            }
            out.push('\n');
        }
        out
    }

    /// The set of known module paths (every import path plus its dotted
    /// ancestors) — the project surface the verify gate resolves against.
    #[must_use]
    pub fn known_modules(&self) -> std::collections::BTreeSet<String> {
        let mut set = std::collections::BTreeSet::new();
        for m in &self.modules {
            for ancestor in dotted_ancestors(&m.import_path) {
                set.insert(ancestor);
            }
        }
        set
    }

    /// Build the authoritative [`SymbolIndex`] the verify oracle resolves against:
    /// every import path (and its dotted ancestors — `newt_agent`,
    /// `newt_agent._newt_agent`, …, all real packages) is a known module, and each
    /// exported symbol is registered under its module.
    #[must_use]
    pub fn to_symbol_index(&self) -> SymbolIndex {
        let mut index = SymbolIndex::new();
        for m in &self.modules {
            for ancestor in dotted_ancestors(&m.import_path) {
                index.add_module(ancestor);
            }
            for symbol in &m.symbols {
                index.add_symbol(m.import_path.clone(), symbol.clone());
            }
        }
        index
    }
}

/// Every dotted prefix of a module path, longest last:
/// `"a.b.c"` → `["a", "a.b", "a.b.c"]`. Each is a real, importable package.
fn dotted_ancestors(path: &str) -> Vec<String> {
    let mut acc = String::new();
    let mut out = Vec::new();
    for part in path.split('.') {
        if !acc.is_empty() {
            acc.push('.');
        }
        acc.push_str(part);
        out.push(acc.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols::{Reference, Resolution};

    // A faithful slice of a real `pyo3_module.rs` (newt-core): a multi-line
    // pyclass attr, a single-line one, and `m.add` / `create_exception` exports.
    const NEWT_CORE_SRC: &str = r#"
create_exception!(_newt_agent, PyNewtError, PyException);

#[pyclass(
    name = "Tier",
    module = "newt_agent._newt_agent.core",
    eq,
)]
pub struct PyTier;

#[pyclass(name = "Router", module = "newt_agent._newt_agent.core")]
pub struct PyRouter;

pub fn register(py: Python<'_>, parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(py, "core")?;
    m.add_class::<PyTier>()?;
    m.add_class::<PyRouter>()?;
    m.add("NewtError", py.get_type::<PyNewtError>())?;
    Ok(())
}
"#;

    const NEWT_DATA_SRC: &str = r#"
#[pyclass(name = "DataStore", module = "newt_agent._newt_agent.data")]
pub struct PyDataStore;
"#;

    #[test]
    fn extracts_import_path_and_symbols() {
        let m = FfiModule::from_source("newt-core", NEWT_CORE_SRC).unwrap();
        assert_eq!(m.source, "newt-core");
        assert_eq!(m.import_path, "newt_agent._newt_agent.core");
        // pyclass names + the m.add export, sorted + deduped.
        assert_eq!(m.symbols, vec!["NewtError", "Router", "Tier"]);
    }

    #[test]
    fn source_without_pyclass_module_is_none() {
        assert!(FfiModule::from_source("plain", "pub fn f() {}").is_none());
        // a pyclass with no `module =` is not authoritatively placeable
        assert!(FfiModule::from_source("nomod", r#"#[pyclass(name = "X")] struct X;"#).is_none());
    }

    #[test]
    fn manifest_skips_unbound_sources() {
        let manifest = FfiManifest::from_sources([
            ("newt-core", NEWT_CORE_SRC),
            ("plain", "pub fn nothing() {}"),
            ("newt-data", NEWT_DATA_SRC),
        ]);
        assert_eq!(manifest.len(), 2);
        assert!(!manifest.is_empty());
        let paths: Vec<&str> = manifest
            .modules
            .iter()
            .map(|m| m.import_path.as_str())
            .collect();
        assert_eq!(
            paths,
            vec!["newt_agent._newt_agent.core", "newt_agent._newt_agent.data"]
        );
    }

    #[test]
    fn from_workspace_discovers_bindings() {
        let tmp = tempfile::tempdir().unwrap();
        // <root>/newt-core/src/pyo3_module.rs and <root>/newt-data/src/...
        for (crate_dir, src) in [("newt-core", NEWT_CORE_SRC), ("newt-data", NEWT_DATA_SRC)] {
            let dir = tmp.path().join(crate_dir).join("src");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("pyo3_module.rs"), src).unwrap();
        }
        // a non-crate dir and a crate without a binding are both ignored
        std::fs::create_dir_all(tmp.path().join("docs")).unwrap();
        std::fs::create_dir_all(tmp.path().join("newt-empty").join("src")).unwrap();

        let manifest = FfiManifest::from_workspace(tmp.path()).unwrap();
        assert_eq!(manifest.len(), 2);
        // deterministic (sorted) order: newt-core before newt-data
        assert_eq!(manifest.modules[0].source, "newt-core");
        assert_eq!(
            manifest.modules[0].import_path,
            "newt_agent._newt_agent.core"
        );
        assert_eq!(manifest.modules[1].source, "newt-data");
    }

    #[test]
    fn render_block_is_the_injectable_fact() {
        let manifest = FfiManifest::from_sources([("newt-core", NEWT_CORE_SRC)]);
        let block = manifest.render_block();
        assert!(block.contains("authoritative"));
        assert!(block.contains("newt-core → newt_agent._newt_agent.core"));
        assert!(block.contains("NewtError, Router, Tier"));
    }

    #[test]
    fn empty_manifest_renders_nothing() {
        let manifest = FfiManifest::default();
        assert!(manifest.is_empty());
        assert_eq!(manifest.render_block(), "");
    }

    #[test]
    fn symbol_index_resolves_real_and_flags_fabricated() {
        let manifest = FfiManifest::from_sources([("newt-core", NEWT_CORE_SRC)]);
        let index = manifest.to_symbol_index();

        // the real submodule path and its ancestors resolve
        assert!(index.has_module("newt_agent._newt_agent.core"));
        assert!(index.has_module("newt_agent._newt_agent"));
        assert!(index.has_module("newt_agent"));
        // a real symbol under the real module resolves
        assert_eq!(
            index.resolve(&Reference::import_from(
                "newt_agent._newt_agent.core",
                "Router",
                1
            )),
            Resolution::Resolved
        );
        // the fabricated crate-name module does NOT exist
        assert_eq!(
            index.resolve(&Reference::import_module("newt_core", 1)),
            Resolution::UnknownModule
        );
    }

    #[test]
    fn dotted_ancestors_are_every_prefix() {
        assert_eq!(
            dotted_ancestors("a.b.c"),
            vec!["a".to_string(), "a.b".to_string(), "a.b.c".to_string()]
        );
        assert_eq!(dotted_ancestors("solo"), vec!["solo".to_string()]);
    }
}
