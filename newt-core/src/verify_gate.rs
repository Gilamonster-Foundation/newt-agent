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
//! Module-level resolution is shared with the scorer via
//! [`module_is_known`](crate::symbols::module_is_known) +
//! [`python_stdlib_modules`](crate::symbols::python_stdlib_modules), so the gate
//! (control) and the verify oracle (measurement) never disagree. Symbol-level
//! follows the FFI manifest (#74).

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

/// `surface` (the project's known modules, e.g. R1's
/// [`FfiManifest::known_modules`](crate::ffi_manifest::FfiManifest::known_modules))
/// unioned with the Python stdlib — the full "known" set the gate resolves
/// against. Built once per gate run.
fn known_set(surface: &BTreeSet<String>) -> BTreeSet<String> {
    let mut known = surface.clone();
    known.extend(python_stdlib_modules());
    known
}

/// Gate one Python source against the project `surface` (stdlib is always known).
/// An import whose module — or a dotted prefix — is neither in `surface` nor the
/// stdlib is a fabrication.
#[must_use]
pub fn gate_python_source(
    path: impl Into<PathBuf>,
    source: &str,
    surface: &BTreeSet<String>,
) -> FileVerdict {
    gate_with_known(path, source, &known_set(surface))
}

/// Gate against a pre-merged `known` set (`surface ∪ stdlib`) — the per-file core
/// the workspace walk calls so the stdlib union is computed once, not per file.
fn gate_with_known(
    path: impl Into<PathBuf>,
    source: &str,
    known: &BTreeSet<String>,
) -> FileVerdict {
    let fabrications = extract_references(source, Lang::Python)
        .into_iter()
        .filter(|r| !module_is_known(&r.module, known))
        .map(|r| Fabrication {
            module: r.module,
            line: r.line,
        })
        .collect();
    FileVerdict {
        path: path.into(),
        fabrications,
    }
}

/// Gate every `.py` file under `workspace` against `surface`. Files are returned
/// in sorted (deterministic) path order; paths are relative to `workspace`.
///
/// # Errors
/// Propagates I/O errors from reading the workspace tree.
pub fn gate_python_workspace(
    workspace: &Path,
    surface: &BTreeSet<String>,
) -> std::io::Result<GateReport> {
    let known = known_set(surface);
    let mut py_files = Vec::new();
    collect_py_files(workspace, &mut py_files)?;
    py_files.sort();

    let mut files = Vec::new();
    for abs in py_files {
        let source = std::fs::read_to_string(&abs)?;
        let rel = abs.strip_prefix(workspace).unwrap_or(&abs).to_path_buf();
        files.push(gate_with_known(rel, &source, &known));
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
