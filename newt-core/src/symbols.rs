//! The verify oracle's symbol index — "does the symbol this code *references*
//! actually exist?"
//!
//! Lives in `newt-core` (not `newt-coder`) on purpose: the S1 verify gate in
//! [`crate::agentic`]'s tool executor and the coder render path (in `newt-coder`,
//! which depends on `newt-core`) must *both* reach it, and `newt-core` cannot
//! depend on `newt-coder`. One oracle, two consumers — see
//! `docs/design/coder-symbolic-memory.md` §6A.
//!
//! This is the **regex-floor, build-free first increment** and is built
//! **general-first**: a language-agnostic [`SymbolIndex`] / [`resolve`] /
//! [`classify`](SymbolIndex::classify) core with a concrete **Python** adapter
//! (the second nemotron incident's language). The Rust adapter and the
//! tree-sitter upgrade are follow-ups; the build-once FFI-introspection manifest
//! for the PyO3 cross-language surface (where a Rust `#[pyclass]` becomes
//! `newt_agent.core.Router`) is tracked separately (#74). What ships here is the
//! cheap tier that catches a *fabricated reference* — the failure `py_compile`
//! is blind to — in microseconds with no compiler.
//!
//! ## The two-stage discrimination (§6A.5)
//!
//! A missing module is ambiguous: the umbrella extension might simply be
//! **not built** (an environment problem, not the model's fault), or the name
//! might be **fabricated** (the real signal). [`SymbolIndex::classify`] takes the
//! set of known-but-unbuilt package roots and separates the two, so a verify gate
//! never reports "fabrication" for a wheel that was never compiled.

use std::collections::BTreeSet;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// A source language the oracle has an adapter for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// Python — `from`/`import` (`.`-separated modules).
    Python,
    /// Rust — `use` paths (`::`-separated modules) + `fn`/`struct`/`enum`/`trait`
    /// declarations. The second wired adapter: same general core, a different
    /// grammar — the "language pack" shape made concrete.
    Rust,
}

impl Lang {
    /// Best-effort language from a file extension. `None` for unwired/unknown.
    #[must_use]
    pub fn from_path(path: &str) -> Option<Self> {
        match path.rsplit('.').next() {
            Some("py") => Some(Self::Python),
            Some("rs") => Some(Self::Rust),
            _ => None,
        }
    }
}

/// A symbol a file **references** — an import or qualified use the oracle checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// The dotted/pathed module being imported from or imported
    /// (`"newt_agent.core"`, `"os"`).
    pub module: String,
    /// The specific symbol imported (`Some("Router")`), or `None` for a bare
    /// `import module` that only asserts the module exists.
    pub name: Option<String>,
    /// 1-based source line, for the gate to point at.
    pub line: usize,
}

impl Reference {
    /// A `from <module> import <name>` reference.
    #[must_use]
    pub fn import_from(module: impl Into<String>, name: impl Into<String>, line: usize) -> Self {
        Self {
            module: module.into(),
            name: Some(name.into()),
            line,
        }
    }

    /// A bare `import <module>` reference (module-existence only).
    #[must_use]
    pub fn import_module(module: impl Into<String>, line: usize) -> Self {
        Self {
            module: module.into(),
            name: None,
            line,
        }
    }
}

/// A symbol a file **defines** — what the index is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    /// The bare declared name (`"Router"`, `"load_csv_to_sqlite"`).
    pub name: String,
    /// What kind of declaration it is.
    pub kind: DefKind,
    /// 1-based source line.
    pub line: usize,
}

/// The kind of a [`Definition`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefKind {
    /// `def` / `fn`.
    Function,
    /// `class`.
    Class,
    /// `struct`.
    Struct,
    /// `enum`.
    Enum,
    /// `trait`.
    Trait,
}

/// The outcome of resolving one [`Reference`] against a [`SymbolIndex`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// The module (and symbol, if named) exists.
    Resolved,
    /// The module exists but the named symbol does not.
    UnknownSymbol,
    /// The module itself is unknown (could be fabrication *or* not-built —
    /// [`SymbolIndex::classify`] disambiguates).
    UnknownModule,
}

/// The verify gate's verdict on one reference, after disambiguating not-built
/// from fabricated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    /// The reference resolves — no problem.
    Resolved,
    /// The reference's package root is known-but-unbuilt: an **environment**
    /// problem (run the build), not a model failure.
    NotBuilt {
        /// The unbuilt package root (e.g. `newt_agent`).
        root: String,
    },
    /// The model **fabricated** a module or symbol that does not exist — the
    /// signal the cheap verify tier exists to catch.
    Fabricated {
        /// Human-readable detail naming what was fabricated.
        detail: String,
    },
}

/// A queryable set of known symbols. Built from extracted [`Definition`]s
/// (same-language) and/or, later, the FFI-introspection manifest (#74).
#[derive(Debug, Clone, Default)]
pub struct SymbolIndex {
    modules: BTreeSet<String>,
    symbols: BTreeSet<(String, String)>,
}

impl SymbolIndex {
    /// An empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that a module exists (no symbols yet).
    pub fn add_module(&mut self, module: impl Into<String>) {
        self.modules.insert(module.into());
    }

    /// Record that `module` exports `symbol` (also marks the module known).
    pub fn add_symbol(&mut self, module: impl Into<String>, symbol: impl Into<String>) {
        let module = module.into();
        self.modules.insert(module.clone());
        self.symbols.insert((module, symbol.into()));
    }

    /// Extract definitions from one file's `source` and add each as a symbol of
    /// `module`. The module path is the caller's (it knows the file's package
    /// location); deriving it from the workspace layout is a later increment.
    pub fn add_from_source(&mut self, module: &str, source: &str, lang: Lang) {
        for def in extract_definitions(source, lang) {
            self.add_symbol(module, def.name);
        }
    }

    /// Is this module path known?
    #[must_use]
    pub fn has_module(&self, module: &str) -> bool {
        self.modules.contains(module)
    }

    /// Resolve one reference against the index.
    #[must_use]
    pub fn resolve(&self, reference: &Reference) -> Resolution {
        if !self.modules.contains(&reference.module) {
            return Resolution::UnknownModule;
        }
        match &reference.name {
            Some(name) => {
                if self
                    .symbols
                    .contains(&(reference.module.clone(), name.clone()))
                {
                    Resolution::Resolved
                } else {
                    Resolution::UnknownSymbol
                }
            }
            None => Resolution::Resolved,
        }
    }

    /// Resolve a reference and disambiguate a missing module into *not-built*
    /// (environment) vs *fabricated* (the model's error), using the set of
    /// package roots known to be present-but-unbuilt.
    #[must_use]
    pub fn classify(&self, reference: &Reference, unbuilt_roots: &BTreeSet<String>) -> Verdict {
        match self.resolve(reference) {
            Resolution::Resolved => Verdict::Resolved,
            Resolution::UnknownSymbol => Verdict::Fabricated {
                detail: format!(
                    "module `{}` has no symbol `{}`",
                    reference.module,
                    reference.name.as_deref().unwrap_or("")
                ),
            },
            Resolution::UnknownModule => {
                let root = module_root(&reference.module);
                if unbuilt_roots.contains(root) {
                    Verdict::NotBuilt {
                        root: root.to_string(),
                    }
                } else {
                    Verdict::Fabricated {
                        detail: format!("no module `{}`", reference.module),
                    }
                }
            }
        }
    }
}

/// The top-level root of a dotted/pathed module (`"newt_agent.core"` →
/// `"newt_agent"`, `"a::b"` → `"a"`).
fn module_root(module: &str) -> &str {
    module.split(['.', ':']).next().unwrap_or(module)
}

/// A module is **known** if it — or any of its dotted prefixes — is in `known`
/// (so a declared `newt_agent` covers `newt_agent.core`, and `os` covers
/// `os.path`). The module-level resolution shared by the verify oracle (scoring)
/// and the verify gate (control), so the two never drift.
#[must_use]
pub fn module_is_known(module: &str, known: &BTreeSet<String>) -> bool {
    let parts: Vec<&str> = module.split('.').collect();
    (1..=parts.len()).any(|i| known.contains(&parts[..i].join(".")))
}

/// A focused allowlist of common Python standard-library top-level modules, so a
/// generated example's `import os` / `from dataclasses import dataclass` is not
/// mistaken for a fabrication. Not exhaustive — errs toward false-negatives (miss
/// a stdlib edge) over false-positives (flag real stdlib).
#[must_use]
pub fn python_stdlib_modules() -> BTreeSet<String> {
    [
        // Dunder modules CPython always provides — `from __future__ import
        // annotations` is the single most common line in modern typed Python.
        "__future__",
        "__main__",
        "abc",
        "argparse",
        "asyncio",
        "base64",
        "collections",
        "contextlib",
        "copy",
        "csv",
        "dataclasses",
        "datetime",
        "decimal",
        "enum",
        "functools",
        "glob",
        "hashlib",
        "io",
        "itertools",
        "json",
        "logging",
        "math",
        "os",
        "pathlib",
        "random",
        "re",
        "shutil",
        "subprocess",
        "sys",
        "tempfile",
        "time",
        "typing",
        "unittest",
        "uuid",
        "warnings",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

/// Extract the references (imports/uses) a source file makes.
#[must_use]
pub fn extract_references(source: &str, lang: Lang) -> Vec<Reference> {
    match lang {
        Lang::Python => extract_references_python(source),
        Lang::Rust => extract_references_rust(source),
    }
}

/// Extract the symbols a source file defines.
#[must_use]
pub fn extract_definitions(source: &str, lang: Lang) -> Vec<Definition> {
    match lang {
        Lang::Python => extract_definitions_python(source),
        Lang::Rust => extract_definitions_rust(source),
    }
}

fn extract_references_python(source: &str) -> Vec<Reference> {
    // `from <module> import a, b as c, d`. The module class includes `-` so a
    // fabricated hyphenated crate name (`from newt-eval import …`) is captured
    // and checked, not silently dropped — even though it is not legal Python.
    let from_re = Regex::new(r"^\s*from\s+([\w.-]+)\s+import\s+(.+?)\s*$").unwrap();
    // `import <module>[ as x][, <module2>...]`
    let import_re = Regex::new(r"^\s*import\s+(.+?)\s*$").unwrap();

    let mut refs = Vec::new();
    for (i, line) in source.lines().enumerate() {
        let lineno = i + 1;
        if let Some(caps) = from_re.captures(line) {
            let module = caps[1].to_string();
            // Relative imports (`from . import x`, `from .sub import y`) are
            // intra-package and always resolvable — never a fabrication. The
            // regex captures the leading dots into `module`, so detect them here.
            if module.starts_with('.') {
                continue;
            }
            let names = caps[2].trim();
            // `from <module> import (` with the names deferred to the following
            // lines (the black/isort multi-line form). The names group is just
            // the open paren; record the module-existence reference now —
            // module-level resolution doesn't need the deferred symbol list.
            if names == "(" {
                refs.push(Reference::import_module(module, lineno));
                continue;
            }
            let before = refs.len();
            for item in names.split(',') {
                let name = item
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_matches(|c| c == '(' || c == ')');
                if name.is_empty() || name == "*" {
                    continue;
                }
                refs.push(Reference::import_from(module.clone(), name, lineno));
            }
            // `from <module> import *` (or any line naming no specific symbol)
            // still asserts the module exists — record the module-existence
            // reference so a fabricated module is caught even via a wildcard.
            if refs.len() == before {
                refs.push(Reference::import_module(module, lineno));
            }
        } else if let Some(caps) = import_re.captures(line) {
            for item in caps[1].split(',') {
                // `module as alias` → take the module.
                let module = item.split_whitespace().next().unwrap_or("");
                if !module.is_empty() {
                    refs.push(Reference::import_module(module, lineno));
                }
            }
        }
    }
    refs
}

fn extract_definitions_python(source: &str) -> Vec<Definition> {
    let def_re = Regex::new(r"^\s*def\s+(\w+)").unwrap();
    let class_re = Regex::new(r"^\s*class\s+(\w+)").unwrap();

    let mut defs = Vec::new();
    for (i, line) in source.lines().enumerate() {
        let lineno = i + 1;
        if let Some(caps) = def_re.captures(line) {
            defs.push(Definition {
                name: caps[1].to_string(),
                kind: DefKind::Function,
                line: lineno,
            });
        } else if let Some(caps) = class_re.captures(line) {
            defs.push(Definition {
                name: caps[1].to_string(),
                kind: DefKind::Class,
                line: lineno,
            });
        }
    }
    defs
}

fn extract_references_rust(source: &str) -> Vec<Reference> {
    // `[pub] use <path>;` — capture the path, then parse its shape. Line-based
    // (the regex-floor sibling of the Python adapter): multi-line `use` blocks
    // and deeply-nested groups are out of scope until the tree-sitter upgrade.
    let use_re = Regex::new(r"^\s*(?:pub\s*(?:\([^)]*\)\s*)?)?use\s+(.+?)\s*;").unwrap();
    let mut refs = Vec::new();
    for (i, line) in source.lines().enumerate() {
        let lineno = i + 1;
        if let Some(caps) = use_re.captures(line) {
            parse_rust_use(caps[1].trim(), lineno, &mut refs);
        }
    }
    refs
}

/// Parse one Rust `use` path into references. Handles `a::b::c`, `a::b::{c, d as
/// e, self, *}`, and `a::b::*`.
fn parse_rust_use(path: &str, lineno: usize, refs: &mut Vec<Reference>) {
    // Grouped: `a::b::{c, d as e, self, *}`.
    if let Some(open) = path.find("::{") {
        let module = &path[..open];
        let inner = path[open + 3..].trim_end_matches('}');
        for item in inner.split(',') {
            // Take the first token so `d as e` becomes `d` (the imported name).
            let item = item.split_whitespace().next().unwrap_or("");
            if item.is_empty() || item == "*" || item == "self" {
                // `*` (glob) and `self` (re-export the module) assert the module,
                // not a specific name.
                refs.push(Reference::import_module(module, lineno));
            } else {
                refs.push(Reference::import_from(module, item, lineno));
            }
        }
        return;
    }
    // Glob: `a::b::*`.
    if let Some(module) = path.strip_suffix("::*") {
        refs.push(Reference::import_module(module, lineno));
        return;
    }
    // Simple: `a::b::c` (optionally `as d`) → module `a::b`, name `c`.
    let head = path.split_whitespace().next().unwrap_or(path);
    if let Some(idx) = head.rfind("::") {
        refs.push(Reference::import_from(
            &head[..idx],
            &head[idx + 2..],
            lineno,
        ));
    } else {
        // A single-ident `use foo;` names a module.
        refs.push(Reference::import_module(head, lineno));
    }
}

fn extract_definitions_rust(source: &str) -> Vec<Definition> {
    // Optional visibility (`pub`, `pub(crate)`, `pub(in ...)`) then the keyword.
    let vis = r"(?:pub\s*(?:\([^)]*\)\s*)?)?";
    let fn_re = Regex::new(&format!(
        r"^\s*{vis}(?:async\s+|unsafe\s+|const\s+|extern\s+(?:\x22[^\x22]*\x22\s+)?)*fn\s+(\w+)"
    ))
    .unwrap();
    let struct_re = Regex::new(&format!(r"^\s*{vis}struct\s+(\w+)")).unwrap();
    let enum_re = Regex::new(&format!(r"^\s*{vis}enum\s+(\w+)")).unwrap();
    let trait_re = Regex::new(&format!(r"^\s*{vis}(?:unsafe\s+)?trait\s+(\w+)")).unwrap();

    let mut defs = Vec::new();
    for (i, line) in source.lines().enumerate() {
        let lineno = i + 1;
        let captured = [
            (&fn_re, DefKind::Function),
            (&struct_re, DefKind::Struct),
            (&enum_re, DefKind::Enum),
            (&trait_re, DefKind::Trait),
        ]
        .into_iter()
        .find_map(|(re, kind)| re.captures(line).map(|c| (c[1].to_string(), kind)));
        if let Some((name, kind)) = captured {
            defs.push(Definition {
                name,
                kind,
                line: lineno,
            });
        }
    }
    defs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn built() -> BTreeSet<String> {
        BTreeSet::new()
    }

    #[test]
    fn oracle_catches_the_second_nemotron_incident() {
        // The deterministic regression for the post-#321 confabulation: the model
        // wrote `from newt_core import classify` / `from newt_data import
        // DataStore` against names that do not exist. The real surface is the
        // umbrella `newt_agent` with submodules.
        let mut idx = SymbolIndex::new();
        idx.add_symbol("newt_agent.core", "Router");
        idx.add_symbol("newt_agent.core", "Tier");
        idx.add_symbol("newt_agent.data", "load_csv_to_sqlite");

        // The fabricated references must be caught.
        let fab1 = Reference::import_from("newt_core", "classify", 1);
        let fab2 = Reference::import_from("newt_data", "DataStore", 2);
        assert!(matches!(
            idx.classify(&fab1, &built()),
            Verdict::Fabricated { .. }
        ));
        assert!(matches!(
            idx.classify(&fab2, &built()),
            Verdict::Fabricated { .. }
        ));

        // The real references must resolve.
        let real = Reference::import_from("newt_agent.core", "Router", 3);
        assert_eq!(idx.classify(&real, &built()), Verdict::Resolved);

        // A real module with a fabricated symbol is still fabrication.
        let bad_symbol = Reference::import_from("newt_agent.core", "classify", 4);
        assert!(matches!(
            idx.classify(&bad_symbol, &built()),
            Verdict::Fabricated { .. }
        ));
    }

    #[test]
    fn not_built_is_distinguished_from_fabricated() {
        // The umbrella is a known package that simply isn't compiled yet.
        let mut idx = SymbolIndex::new();
        idx.add_module("newt_agent");
        let unbuilt: BTreeSet<String> = ["newt_agent".to_string()].into_iter().collect();

        // A submodule of the unbuilt umbrella → environment, not the model's fault.
        let r = Reference::import_from("newt_agent.core", "Router", 1);
        assert_eq!(
            idx.classify(&r, &unbuilt),
            Verdict::NotBuilt {
                root: "newt_agent".to_string()
            }
        );

        // A wholly fictional package is fabrication regardless of build state.
        let fab = Reference::import_from("newt_core", "classify", 2);
        assert!(matches!(
            idx.classify(&fab, &unbuilt),
            Verdict::Fabricated { .. }
        ));
    }

    #[test]
    fn extracts_python_imports() {
        let src = "from newt_agent.core import Router, Tier\nimport os\nimport newt_data as nd\nfrom x import *\n";
        let refs = extract_references(src, Lang::Python);
        assert!(refs.contains(&Reference::import_from("newt_agent.core", "Router", 1)));
        assert!(refs.contains(&Reference::import_from("newt_agent.core", "Tier", 1)));
        assert!(refs.contains(&Reference::import_module("os", 2)));
        assert!(refs.contains(&Reference::import_module("newt_data", 3)));
        // wildcard asserts nothing about a name
        assert!(!refs.iter().any(|r| r.name.as_deref() == Some("*")));
    }

    #[test]
    fn extracts_python_definitions() {
        let src = "def foo():\n    pass\n\nclass Bar:\n    pass\n";
        let defs = extract_definitions(src, Lang::Python);
        assert!(defs
            .iter()
            .any(|d| d.name == "foo" && d.kind == DefKind::Function));
        assert!(defs
            .iter()
            .any(|d| d.name == "Bar" && d.kind == DefKind::Class));
    }

    #[test]
    fn add_from_source_builds_a_resolvable_index() {
        let mut idx = SymbolIndex::new();
        idx.add_from_source(
            "mypkg.mod",
            "def helper():\n    pass\nclass Widget:\n    pass\n",
            Lang::Python,
        );
        assert_eq!(
            idx.resolve(&Reference::import_from("mypkg.mod", "Widget", 1)),
            Resolution::Resolved
        );
        assert_eq!(
            idx.resolve(&Reference::import_from("mypkg.mod", "Nope", 1)),
            Resolution::UnknownSymbol
        );
        assert_eq!(
            idx.resolve(&Reference::import_from("other.mod", "Widget", 1)),
            Resolution::UnknownModule
        );
    }

    #[test]
    fn bare_import_only_checks_module_existence() {
        let mut idx = SymbolIndex::new();
        idx.add_module("os");
        assert_eq!(
            idx.resolve(&Reference::import_module("os", 1)),
            Resolution::Resolved
        );
        assert_eq!(
            idx.resolve(&Reference::import_module("nope", 1)),
            Resolution::UnknownModule
        );
    }

    #[test]
    fn extracts_rust_use_statements() {
        let src = "use a::b::Thing;\n\
                   pub use c::d::{One, Two as T, self, *};\n\
                   use crate::scope::*;\n\
                   use std::collections::HashMap as Map;\n";
        let refs = extract_references(src, Lang::Rust);
        assert!(refs.contains(&Reference::import_from("a::b", "Thing", 1)));
        assert!(refs.contains(&Reference::import_from("c::d", "One", 2)));
        assert!(refs.contains(&Reference::import_from("c::d", "Two", 2))); // `as T` -> the item `Two`
        assert!(refs.contains(&Reference::import_module("c::d", 2))); // `self` and `*`
        assert!(refs.contains(&Reference::import_module("crate::scope", 3)));
        assert!(refs.contains(&Reference::import_from("std::collections", "HashMap", 4)));
    }

    #[test]
    fn extracts_rust_definitions() {
        let src = "pub fn alpha() {}\n\
                   async fn beta() {}\n\
                   pub(crate) struct Gamma { x: u8 }\n\
                   enum Delta { A, B }\n\
                   pub trait Epsilon {}\n\
                   const NOT_A_DEF: u8 = 0;\n";
        let defs = extract_definitions(src, Lang::Rust);
        assert!(defs
            .iter()
            .any(|d| d.name == "alpha" && d.kind == DefKind::Function));
        assert!(defs
            .iter()
            .any(|d| d.name == "beta" && d.kind == DefKind::Function));
        assert!(defs
            .iter()
            .any(|d| d.name == "Gamma" && d.kind == DefKind::Struct));
        assert!(defs
            .iter()
            .any(|d| d.name == "Delta" && d.kind == DefKind::Enum));
        assert!(defs
            .iter()
            .any(|d| d.name == "Epsilon" && d.kind == DefKind::Trait));
        // `const` is not a floor def-kind — not extracted.
        assert!(!defs.iter().any(|d| d.name == "NOT_A_DEF"));
    }

    #[test]
    fn rust_references_resolve_against_the_same_general_core() {
        // The language-pack payoff: the SAME SymbolIndex/resolve/classify core
        // grades Rust via a different extractor — fabricated `use` paths are
        // caught exactly as fabricated Python imports are.
        let mut idx = SymbolIndex::new();
        idx.add_symbol("crate::router", "Router");
        let real = &extract_references("use crate::router::Router;", Lang::Rust)[0];
        assert_eq!(idx.resolve(real), Resolution::Resolved);
        let fab = &extract_references("use crate::nonsense::Widget;", Lang::Rust)[0];
        assert!(matches!(
            idx.classify(fab, &built()),
            Verdict::Fabricated { .. }
        ));
    }

    #[test]
    fn module_root_handles_python_and_rust_paths() {
        assert_eq!(module_root("newt_agent.core"), "newt_agent");
        assert_eq!(module_root("a::b::c"), "a");
        assert_eq!(module_root("flat"), "flat");
    }

    #[test]
    fn lang_from_path() {
        assert_eq!(Lang::from_path("foo/bar.py"), Some(Lang::Python));
        assert_eq!(Lang::from_path("src/lib.rs"), Some(Lang::Rust));
        assert_eq!(Lang::from_path("README.md"), None);
    }

    #[test]
    fn verdict_serializes_with_a_tag() {
        let v = Verdict::Fabricated {
            detail: "no module `x`".to_string(),
        };
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains("\"verdict\":\"fabricated\""));
    }
}
