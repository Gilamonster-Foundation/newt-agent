//! The verify gate (#73) — the harness's red-squiggle for fabricated imports,
//! and R2's decision core.
//!
//! After a coding turn, resolve the produced Python files' imports against the
//! authoritative surface (the R1 [`FfiManifest`](crate::ffi_manifest::FfiManifest)
//! `known_modules()`, or any module set) and decide which files to revert. The
//! decision is **file-scoped**: the with-manifest runs
//! (`docs/findings/2026-06-14-fabrication-...`) showed nemotron, even when handed
//! the surface, *hedges* — writing whole grounded files alongside whole
//! fabricated ones. Reverting the fabricated **file** (and retrying just it) is
//! the surgical fix, and it composes with R1: R1 raises per-import grounding, the
//! gate deletes the residual fabrications.
//!
//! Resolution shares its primitives with the scorer
//! ([`module_is_known`](crate::symbols::module_is_known),
//! [`python_stdlib_modules`](crate::symbols::python_stdlib_modules)) but the gate
//! is deliberately **stricter**: as a *control* signal in a retry loop it matches
//! the project surface leaf-[`Exact`](SurfaceMatch::Exact) by default, where the
//! scorer prefix-matches its coarser hand-written surface. The retry-Goodhart
//! finding (`docs/findings/2026-06-15-retry-and-the-honest-gate.md`) is why: a
//! control gate must be adversarially complete or the model games its blind
//! spots. Symbol-level resolution still follows the FFI manifest (#74).

use crate::symbols::{extract_references, module_is_known, python_stdlib_modules, Lang};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// One fabricated reference: the module imported and the line it sat on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fabrication {
    /// The fabricated module path (e.g. `"newt_core"`).
    pub module: String,
    /// 1-based source line, so the gate can point at it.
    pub line: usize,
}

/// The gate's verdict for one produced file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileVerdict {
    /// The file, relative to the gated workspace root.
    pub path: PathBuf,
    /// Fabricated module imports in this file — empty iff the file is clean.
    pub fabrications: Vec<Fabrication>,
}

impl FileVerdict {
    /// No fabricated imports — the file is accepted as-is.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.fabrications.is_empty()
    }
}

/// The gate's decision over a produced workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GateReport {
    /// One entry per gated file, in sorted path order.
    pub files: Vec<FileVerdict>,
}

impl GateReport {
    /// The revert set — files with at least one fabricated import. These are the
    /// files R2 reverts and retries; a clean file is never touched.
    #[must_use]
    pub fn revert_set(&self) -> Vec<&Path> {
        self.files
            .iter()
            .filter(|f| !f.is_clean())
            .map(|f| f.path.as_path())
            .collect()
    }

    /// Accept the turn as-is — true iff no file fabricated.
    #[must_use]
    pub fn accept(&self) -> bool {
        self.files.iter().all(FileVerdict::is_clean)
    }

    /// Total fabricated imports across all files.
    #[must_use]
    pub fn fabrication_count(&self) -> usize {
        self.files.iter().map(|f| f.fabrications.len()).sum()
    }
}

/// How strictly the project surface is matched — a tunable knob.
///
/// The retry-Goodhart finding
/// (`docs/findings/2026-06-15-retry-and-the-honest-gate.md`) showed `Prefix` is
/// exploitable once the gate *controls* a retry loop rather than merely
/// *measuring*: `newt_agent._newt_core` passes via the real `newt_agent` prefix,
/// so the model, under retry pressure, drifts into that blind spot. `Exact` (the
/// default) requires the import to be a module the surface actually declares —
/// sound because R1's manifest carries the full leaf+ancestor set. A coarse,
/// hand-written surface that lists only roots may still want `Prefix`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SurfaceMatch {
    /// The module must be an exact member of the project surface.
    #[default]
    Exact,
    /// The module — or any dotted prefix — is in the surface (lax; legacy).
    Prefix,
}

/// Is `module` resolvable? The project `surface` is matched per `mode`; the
/// Python stdlib is always prefix-matched (`os` covers `os.path`).
fn module_resolves(
    module: &str,
    surface: &BTreeSet<String>,
    stdlib: &BTreeSet<String>,
    mode: SurfaceMatch,
) -> bool {
    let in_surface = match mode {
        SurfaceMatch::Exact => surface.contains(module),
        SurfaceMatch::Prefix => module_is_known(module, surface),
    };
    in_surface || module_is_known(module, stdlib)
}

/// Gate one Python source against the project `surface` with the default
/// (`Exact`) strictness; the Python stdlib is always known.
#[must_use]
pub fn gate_python_source(
    path: impl Into<PathBuf>,
    source: &str,
    surface: &BTreeSet<String>,
) -> FileVerdict {
    gate_python_source_with(path, source, surface, SurfaceMatch::default())
}

/// Gate one Python source, choosing the surface-match strictness.
#[must_use]
pub fn gate_python_source_with(
    path: impl Into<PathBuf>,
    source: &str,
    surface: &BTreeSet<String>,
    mode: SurfaceMatch,
) -> FileVerdict {
    gate_inner(path, source, surface, &python_stdlib_modules(), mode)
}

/// Per-file core. The stdlib set is passed in so the workspace walk computes it
/// once, not per file.
fn gate_inner(
    path: impl Into<PathBuf>,
    source: &str,
    surface: &BTreeSet<String>,
    stdlib: &BTreeSet<String>,
    mode: SurfaceMatch,
) -> FileVerdict {
    let mut fabrications: Vec<Fabrication> = extract_references(source, Lang::Python)
        .into_iter()
        .filter(|r| !module_resolves(&r.module, surface, stdlib, mode))
        .map(|r| Fabrication {
            module: r.module,
            line: r.line,
        })
        .collect();
    // One fabricated module imported as several symbols on one line is one
    // fabrication, not one-per-symbol. References from a line are emitted
    // consecutively, so a consecutive-dedup on (module, line) suffices.
    fabrications.dedup_by(|a, b| a.module == b.module && a.line == b.line);
    FileVerdict {
        path: path.into(),
        fabrications,
    }
}

/// Gate every `.py` file under `workspace` against `surface` with the default
/// (`Exact`) strictness. Files are returned in sorted (deterministic) path order;
/// paths are relative to `workspace`.
///
/// # Errors
/// Propagates I/O errors from reading the workspace tree.
pub fn gate_python_workspace(
    workspace: &Path,
    surface: &BTreeSet<String>,
) -> std::io::Result<GateReport> {
    gate_python_workspace_with(workspace, surface, SurfaceMatch::default())
}

/// Gate every `.py` file under `workspace`, choosing the surface-match strictness.
///
/// # Errors
/// Propagates I/O errors from reading the workspace tree.
pub fn gate_python_workspace_with(
    workspace: &Path,
    surface: &BTreeSet<String>,
    mode: SurfaceMatch,
) -> std::io::Result<GateReport> {
    let stdlib = python_stdlib_modules();
    let mut py_files = Vec::new();
    collect_py_files(workspace, &mut py_files)?;
    py_files.sort();

    let mut files = Vec::new();
    for abs in py_files {
        let source = std::fs::read_to_string(&abs)?;
        let rel = abs.strip_prefix(workspace).unwrap_or(&abs).to_path_buf();
        files.push(gate_inner(rel, &source, surface, &stdlib, mode));
    }
    Ok(GateReport { files })
}

/// Recursively collect `*.py` paths under `dir`.
fn collect_py_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_py_files(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "py") {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi_manifest::FfiManifest;

    const NEWT_CORE_SRC: &str = r#"
#[pyclass(name = "Router", module = "newt_agent._newt_agent.core")]
pub struct PyRouter;
"#;

    fn surface() -> BTreeSet<String> {
        FfiManifest::from_sources([("newt-core", NEWT_CORE_SRC)]).known_modules()
    }

    #[test]
    fn clean_file_has_no_fabrications() {
        // a real submodule import + a stdlib import — both known
        let v = gate_python_source(
            "ok.py",
            "from newt_agent._newt_agent.core import Router\nimport os\nimport os.path\n",
            &surface(),
        );
        assert!(v.is_clean(), "fabrications: {:?}", v.fabrications);
    }

    #[test]
    fn fabricated_import_is_flagged_with_line() {
        let v = gate_python_source("bad.py", "import os\nimport newt_core\n", &surface());
        assert_eq!(v.fabrications.len(), 1);
        assert_eq!(v.fabrications[0].module, "newt_core");
        assert_eq!(v.fabrications[0].line, 2); // points at the offending line
    }

    #[test]
    fn report_revert_set_is_only_fabricating_files() {
        let s = surface();
        let report = GateReport {
            files: vec![
                gate_python_source(
                    "grounded.py",
                    "from newt_agent._newt_agent.core import Router\n",
                    &s,
                ),
                gate_python_source("fab.py", "import newt_core\n", &s),
            ],
        };
        assert!(!report.accept());
        assert_eq!(report.fabrication_count(), 1);
        let revert = report.revert_set();
        assert_eq!(revert.len(), 1);
        assert_eq!(revert[0], Path::new("fab.py"));
    }

    // ── adversarial regressions (false positives / negatives) ──────────

    #[test]
    fn relative_imports_are_not_fabrications() {
        // intra-package; the gate must never revert these (BLOCKER if it does)
        let v = gate_python_source(
            "pkg.py",
            "from . import config\nfrom .helpers import load\nfrom ..util import x\n",
            &surface(),
        );
        assert!(
            v.is_clean(),
            "relative imports flagged: {:?}",
            v.fabrications
        );
    }

    #[test]
    fn future_import_is_not_a_fabrication() {
        let v = gate_python_source(
            "typed.py",
            "from __future__ import annotations\nimport os\n",
            &surface(),
        );
        assert!(v.is_clean(), "__future__ flagged: {:?}", v.fabrications);
    }

    #[test]
    fn realistic_clean_file_is_accepted() {
        // the compound case: future + relative + stdlib + a real PyO3 import
        let v = gate_python_source(
            "real.py",
            "from __future__ import annotations\n\
             from . import config\n\
             from .helpers import load\n\
             import json\n\
             from newt_agent._newt_agent.core import Router\n",
            &surface(),
        );
        assert!(v.is_clean(), "clean file reverted: {:?}", v.fabrications);
    }

    // ── the three retry-Goodhart evasions (#357), now caught ───────────

    #[test]
    fn prefix_breadth_evasion_caught_in_exact_caught_lax_in_prefix() {
        // `newt_agent._newt_core` is fabricated (real leaf is _newt_agent.core)
        // but shares the real `newt_agent` root. Exact (default) catches it;
        // Prefix (the legacy knob) is the documented blind spot.
        let src = "from newt_agent._newt_core import pyo3_module\n";
        let exact = gate_python_source(/* default Exact */ "e.py", src, &surface());
        assert_eq!(exact.fabrications.len(), 1, "Exact must catch it");
        assert_eq!(exact.fabrications[0].module, "newt_agent._newt_core");

        let lax = gate_python_source_with("p.py", src, &surface(), SurfaceMatch::Prefix);
        assert!(lax.is_clean(), "Prefix is the documented lax knob");
    }

    #[test]
    fn hyphen_fabrication_is_caught() {
        // `from newt-eval import …` — the hyphen used to escape the regex
        let v = gate_python_source("h.py", "from newt-eval import pyo3_module\n", &surface());
        assert_eq!(v.fabrications.len(), 1, "got: {:?}", v.fabrications);
        assert_eq!(v.fabrications[0].module, "newt-eval");
    }

    #[test]
    fn wildcard_fabrication_is_caught() {
        // `from <fab> import *` — wildcard used to emit zero references
        let v = gate_python_source("w.py", "from newt_data.pyo3_module import *\n", &surface());
        assert_eq!(v.fabrications.len(), 1, "got: {:?}", v.fabrications);
        assert_eq!(v.fabrications[0].module, "newt_data.pyo3_module");
    }

    #[test]
    fn grounded_wildcard_is_clean() {
        // a wildcard of a REAL module must still pass
        let v = gate_python_source(
            "gw.py",
            "from newt_agent._newt_agent.core import *\n",
            &surface(),
        );
        assert!(
            v.is_clean(),
            "grounded wildcard flagged: {:?}",
            v.fabrications
        );
    }

    #[test]
    fn multiline_paren_fabricated_module_is_caught() {
        // the black/isort open-paren form must not slip past the gate
        let v = gate_python_source(
            "evade.py",
            "from newt_db import (\n    Alpha,\n    Beta,\n)\n",
            &surface(),
        );
        assert_eq!(v.fabrications.len(), 1, "got: {:?}", v.fabrications);
        assert_eq!(v.fabrications[0].module, "newt_db");
    }

    #[test]
    fn one_fabricated_module_many_symbols_counts_once() {
        // single-line parenthesized import of a fabricated module → one fabrication
        let v = gate_python_source(
            "multi.py",
            "from newt_db import (Alpha, Beta, Gamma)\n",
            &surface(),
        );
        assert_eq!(v.fabrications.len(), 1, "got: {:?}", v.fabrications);
    }

    #[test]
    fn gate_workspace_walks_and_is_relative() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("examples")).unwrap();
        std::fs::write(
            tmp.path().join("examples/grounded.py"),
            "from newt_agent._newt_agent.core import Router\nimport json\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("examples/fab.py"), "import newt_coder\n").unwrap();

        let report = gate_python_workspace(tmp.path(), &surface()).unwrap();
        assert_eq!(report.files.len(), 2);
        assert!(!report.accept());
        assert_eq!(report.revert_set(), vec![Path::new("examples/fab.py")]);
    }
}
