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

use async_trait::async_trait;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::symbols::{extract_references, module_is_known, python_stdlib_modules, Lang};
use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SurfaceMatch {
    /// The module must be an exact member of the project surface.
    #[default]
    Exact,
    /// The module — or any dotted prefix — is in the surface (lax; legacy).
    Prefix,
}

/// How strictly the verify gate acts on a turn's flagged output — the **tier**.
/// `RevertRetry` is today's behavior; lower tiers trade enforcement for latitude
/// (a per-model/profile knob).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyTier {
    /// Gate disabled — flagged output is left as-is.
    Off,
    /// Measure + warn only; never revert (non-destructive).
    Advisory,
    /// Revert the flagged files; do not re-prompt.
    RevertOnce,
    /// Revert the flagged files AND issue a corrective retry (today's behavior).
    #[default]
    RevertRetry,
}

/// What a [`VerifyTier`] decides to do, given a [`GateReport`]. A **pure decision**
/// — the caller (the turn loop) executes the revert/retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyAction {
    /// Nothing flagged (or the gate is `Off`) — accept the turn.
    Pass,
    /// Report the fabrications; leave the files in place (advisory).
    Warn,
    /// Revert the flagged files; do not retry.
    Revert,
    /// Revert the flagged files and issue a corrective re-prompt.
    RevertAndRetry,
}

impl VerifyTier {
    /// The action this tier takes for `report` (pure; the caller executes it).
    #[must_use]
    pub fn action(self, report: &GateReport) -> VerifyAction {
        if report.accept() {
            return VerifyAction::Pass; // nothing fabricated
        }
        match self {
            Self::Off => VerifyAction::Pass,
            Self::Advisory => VerifyAction::Warn,
            Self::RevertOnce => VerifyAction::Revert,
            Self::RevertRetry => VerifyAction::RevertAndRetry,
        }
    }
}

/// The honest end-of-turn banner. A turn that did **not** finish cleanly — because
/// the verify gate reverted work (`reverted`), and/or the tool-round cap was hit
/// (`cap_hit`) — gets ONE `needs human review` line naming *both* facts, never a
/// false success. Returns `None` when the turn finished cleanly (nothing reverted,
/// no cap hit). `gave_up` marks revert-retry exhaustion.
#[must_use]
pub fn turn_verdict_banner(reverted: &[String], gave_up: bool, cap_hit: bool) -> Option<String> {
    if reverted.is_empty() && !cap_hit {
        return None;
    }
    let mut parts = Vec::new();
    if !reverted.is_empty() {
        parts.push(format!(
            "the verify gate reverted {} file(s) [{}]{}",
            reverted.len(),
            reverted.join(", "),
            if gave_up {
                " after exhausting retries"
            } else {
                ""
            }
        ));
    }
    if cap_hit {
        parts.push("the tool-round cap was reached".to_string());
    }
    Some(format!(
        "needs human review: {} — the turn did not finish cleanly.",
        parts.join("; and ")
    ))
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

/// Directory names never walked by the gate: dependency/build trees that are not
/// the project's own source. Walking them gates thousands of installed `.py` (perf)
/// and would let the gate *report* fabrications in code newt never wrote (noise).
/// The `retry` revert is already fenced to newt's own writes by the ledger, but
/// keeping these out of the walk is defense-in-depth and a real speedup.
///
/// **Shared, not copied** (#1945). `agentic::self_verify` needs the same
/// judgement for the same reason — a `Cargo.toml` under `target/` or
/// `node_modules/` is a dependency's, not the task's — and the workspace
/// already carries two hand-rolled `starts_with('.') || "target" ||
/// "node_modules"` predicates (`newt-tools::search`, `navigator::text`). A
/// third would be the shape the ratchets exist to catch, so the ignore-set is
/// data with one home and the gates that need it read it here.
pub(crate) const SKIP_DIRS: &[&str] = &[
    ".git",
    ".venv",
    "venv",
    "site-packages",
    "node_modules",
    "target",
    "__pycache__",
    ".tox",
    ".mypy_cache",
];

/// Recursively collect `*.py` paths under `dir`.
///
/// **Symlinks are not followed** — neither symlinked directories nor symlinked
/// files are entered or collected. This is a hard safety boundary: a symlinked dir
/// could otherwise pull external `.py` into the gate (and, were they ever reverted,
/// let a write/delete escape the workspace). Vendored/build dirs ([`SKIP_DIRS`]) are
/// skipped too.
fn collect_py_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        // `file_type()` does NOT traverse the final symlink, so a symlinked dir or
        // file is identified as a symlink here and skipped before we ever follow it.
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        let path = entry.path();
        if ft.is_dir() {
            let skip = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| SKIP_DIRS.contains(&n));
            if !skip {
                collect_py_files(&path, out)?;
            }
        } else if path.extension().is_some_and(|e| e == "py") {
            out.push(path);
        }
    }
    Ok(())
}

// ── The `retry` technique (R2's action arm) ─────────────────────────────────
//
// `verify_gate` (above) *measures*; `retry` *acts* on the measurement: it reverts
// exactly [`GateReport::revert_set`] to its pre-turn state and re-prompts the
// model, up to a cap, then gives up honestly. Contract:
// `docs/design/retry-technique.md`. This module is the pure mechanism (the live
// loop wiring threads [`WriteLedger`] through the write-tool seam separately).
//
// NB: the technique is *named* `retry` in a profile's config, but lives here —
// **not** in `crate::retry` (that is the unrelated HTTP backoff module).

/// A turn-scoped copy-on-first-write ledger: the record that lets `retry` revert a
/// fabricated file to the state it had **before the turn began**.
///
/// The first time a path is written during a turn, its prior bytes are captured
/// (`Some` = the file existed; `None` = it did not). Later writes to the same path
/// in the same turn do **not** overwrite that entry — the *pre-turn* state is the
/// revert target, never an intermediate write. The rig instrument reverted with a
/// bare `rm` (sound only because its corpus files never pre-existed); in the real
/// loop a flagged file may be an *edit* of a tracked file, so the ledger restores
/// bytes rather than deleting. Git-independent by design — newt gates non-git
/// trees too (`docs/design/retry-technique.md`).
#[derive(Debug, Default, Clone)]
pub struct WriteLedger {
    /// Absolute path → pre-turn content (`None` ⇒ the path did not exist pre-turn).
    entries: BTreeMap<PathBuf, Option<Vec<u8>>>,
}

impl WriteLedger {
    /// An empty ledger for a fresh turn.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `path`'s pre-turn state the **first** time it is written this turn.
    /// Call this *before* the write lands. A no-op on any subsequent write to the
    /// same path — the pre-turn snapshot is preserved.
    ///
    /// Only a genuine `NotFound` records the "did-not-exist" marker (`None`) that
    /// makes revert *delete* the path. Any **other** read error (`EACCES`, a
    /// transient NFS hiccup, the path being a directory) leaves the path
    /// **untracked** — conflating "unreadable" with "absent" would let revert delete
    /// a pre-existing file whose note-time read merely failed. Untracked ⇒ revert
    /// returns `false` and refuses to touch it.
    pub fn note_before_write(&mut self, path: impl Into<PathBuf>) {
        let path = path.into();
        if self.entries.contains_key(&path) {
            return;
        }
        let prior = match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return, // unreadable ≠ absent: never mark it delete-on-revert
        };
        self.entries.insert(path, prior);
    }

    /// Restore `path` to its recorded pre-turn state: write the captured bytes
    /// back, or remove the file if it did not exist pre-turn. Returns `true` iff
    /// the path had a ledger entry (so the caller can warn on a gate-flagged path
    /// the ledger never saw — a bug, not a silent delete of an untracked file).
    ///
    /// # Errors
    /// Propagates I/O errors from the restore write or remove (a `NotFound` on
    /// remove is treated as already-reverted, not an error).
    pub fn revert(&self, path: &Path) -> std::io::Result<bool> {
        match self.entries.get(path) {
            None => Ok(false),
            Some(None) => match std::fs::remove_file(path) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
                Err(e) => Err(e),
            },
            Some(Some(bytes)) => {
                std::fs::write(path, bytes)?;
                Ok(true)
            }
        }
    }

    /// Whether nothing has been written this turn.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many distinct paths were written this turn.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// One corrective model turn, abstracted so the loop is testable without a live
/// backend. The implementor applies `corrective_prompt`, writes any produced files
/// into the workspace, **and records each write in the shared [`WriteLedger`]** (so
/// a file a retry newly creates can itself be reverted by the next iteration).
///
/// `?Send` because the live caller is the single-threaded TUI loop (`run_chat`
/// drives each step through a discrete `block_on`); the future never crosses
/// threads.
#[async_trait(?Send)]
pub trait RetryRerun {
    /// Run one grounded re-attempt for the given corrective prompt.
    ///
    /// # Errors
    /// Returns the backend/turn error; `apply_revert_retry` propagates it (a failed
    /// re-attempt aborts the loop rather than masking the failure).
    async fn rerun(&mut self, corrective_prompt: String) -> anyhow::Result<()>;
}

/// The result of a verify-gated revert-retry loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryOutcome {
    /// `true` iff the workspace ended clean (the gate accepts) within the cap.
    pub accepted: bool,
    /// How many re-attempts were actually run (`0..=max_retries`).
    pub retries_used: u32,
    /// Files left reverted at give-up, relative to the workspace (empty iff
    /// `accepted`).
    pub reverted: Vec<PathBuf>,
    /// The fabricated modules still outstanding at give-up, de-duplicated and
    /// sorted (empty iff `accepted`).
    pub outstanding_modules: Vec<String>,
}

impl RetryOutcome {
    fn accepted(retries_used: u32) -> Self {
        Self {
            accepted: true,
            retries_used,
            reverted: Vec::new(),
            outstanding_modules: Vec::new(),
        }
    }
}

/// The distinct fabricated modules across a report, de-duplicated and sorted.
fn outstanding_modules(report: &GateReport) -> Vec<String> {
    report
        .files
        .iter()
        .flat_map(|f| f.fabrications.iter().map(|x| x.module.clone()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The grounded re-prompt: name every reverted file and the modules it fabricated,
/// then hand the model the authoritative surface and forbid the bad imports. One
/// message covers **all** reverted files — a single corrective turn, not a storm.
/// `surface_block` is R1's rendered surface (`FfiManifest::render_block`), the same
/// authority `knowledge_base` injects; `retry` composes with it but does not
/// require it (the surface can ride this message alone).
#[must_use]
pub fn corrective_prompt(report: &GateReport, surface_block: &str) -> String {
    let mut out = String::new();
    out.push_str(
        "Your previous attempt imported modules this project does not expose, so \
         those files were reverted. Fix and rewrite them.\n\n",
    );
    for f in report.files.iter().filter(|f| !f.is_clean()) {
        for fab in &f.fabrications {
            out.push_str(&format!(
                "- `{}` imported `{}` (line {}), which does not exist.\n",
                f.path.display(),
                fab.module,
                fab.line
            ));
        }
    }
    if !surface_block.is_empty() {
        out.push_str("\nThe authoritative import surface is:\n\n");
        out.push_str(surface_block);
        out.push('\n');
    }
    out.push_str(
        "\nRewrite the reverted file(s) using only modules from that surface. Do \
         not import the modules listed above.",
    );
    out
}

/// The authoritative surface the retry loop gates and grounds against — the three
/// facets of "the project's real import surface", grouped so the loop's signature
/// stays small.
pub struct RetrySurface<'a> {
    /// The module set the gate resolves imports against (R1's `known_modules()`).
    pub modules: &'a BTreeSet<String>,
    /// Strictness of the project-surface match (`Exact` default; see [`SurfaceMatch`]).
    pub mode: SurfaceMatch,
    /// R1's rendered surface block (`FfiManifest::render_block`), injected into the
    /// corrective prompt to ground the re-attempt.
    pub block: &'a str,
}

/// Is `abs` inside `ws_canon` (the canonicalized workspace root)? Conservative: a
/// path that cannot be resolved relative to a known root is treated as **outside**.
/// When the workspace root itself cannot be canonicalized (`None`), the check is
/// skipped — the ledger fence remains the primary safety — and the path is allowed.
fn path_within(abs: &Path, ws_canon: Option<&Path>) -> bool {
    let Some(root) = ws_canon else {
        return true;
    };
    // The file usually exists at revert time; canonicalize it directly when it does.
    if let Ok(c) = abs.canonicalize() {
        return c.starts_with(root);
    }
    // Otherwise resolve the parent (which must exist) and re-append the file name —
    // this still defeats a `..`/symlink escape in the parent chain.
    match (abs.parent(), abs.file_name()) {
        (Some(parent), Some(name)) => match parent.canonicalize() {
            Ok(pc) => pc.join(name).starts_with(root),
            Err(_) => false,
        },
        _ => false,
    }
}

/// Drive the verify-gated revert-retry loop: revert the gate's flagged set to its
/// pre-turn state, re-prompt the model with the fabrications named, re-gate, and
/// repeat up to `max_retries` — accepting as soon as the gate is clean, and giving
/// up **honestly** (files left reverted, modules reported) at the cap. `max_retries`
/// is the sole termination authority; a turn runs at most `1 + max_retries` model
/// calls.
///
/// `initial` is the report that already decided retry was needed (from the post-turn
/// `verify_gate` pass). `ledger` is shared with `rerun` via [`RefCell`] so a file a
/// retry creates is itself tracked for the next iteration. `surface` re-gates each
/// attempt and grounds the re-prompt.
///
/// # Errors
/// Propagates a re-gate I/O error or a `rerun` failure (a failed re-attempt aborts
/// the loop rather than reporting a false accept).
pub async fn apply_revert_retry(
    workspace: &Path,
    surface: &RetrySurface<'_>,
    ledger: &RefCell<WriteLedger>,
    initial: GateReport,
    max_retries: u32,
    rerun: &mut dyn RetryRerun,
) -> anyhow::Result<RetryOutcome> {
    // The workspace boundary, resolved once: nothing outside it is ever written or
    // removed, even if a ledger path somehow resolves there (defence-in-depth on
    // top of the ledger fence and the symlink-free walk).
    let ws_canon = workspace.canonicalize().ok();
    let mut report = initial;
    let mut retries_used = 0u32;
    loop {
        if report.accept() {
            return Ok(RetryOutcome::accepted(retries_used));
        }

        // Revert the flagged set to its pre-turn state. The ledger fences this to
        // files **newt itself wrote** this turn: a flagged path with no ledger entry
        // (a pre-existing file, build output, or anything newt did not author) is
        // skipped — never deleted. A clean file the model wrote the same turn is not
        // in `revert_set()`, so it is kept. `reverted` records only what was actually
        // acted on, so the outcome (and the caller's banner) is honest.
        let mut reverted: Vec<PathBuf> = Vec::new();
        {
            let led = ledger.borrow();
            for rel in report.revert_set() {
                let abs = workspace.join(rel);
                if !path_within(&abs, ws_canon.as_deref()) {
                    tracing::warn!(
                        path = %rel.display(),
                        "retry: refusing to revert a path outside the workspace"
                    );
                    continue;
                }
                match led.revert(&abs)? {
                    true => reverted.push(rel.to_path_buf()),
                    false => tracing::warn!(
                        path = %rel.display(),
                        "retry: gate-flagged path not in the write ledger — skipping (will not \
                         delete a file newt did not write)"
                    ),
                }
            }
        }

        if retries_used >= max_retries {
            // Honest give-up: the labelled absence (files left reverted) beats a
            // fabricated presence. The caller surfaces the banner + `retry_exhausted`.
            return Ok(RetryOutcome {
                accepted: false,
                retries_used,
                reverted,
                outstanding_modules: outstanding_modules(&report),
            });
        }

        let prompt = corrective_prompt(&report, surface.block);
        rerun.rerun(prompt).await?;
        retries_used += 1;
        report = gate_python_workspace_with(workspace, surface.modules, surface.mode)?;
    }
}

/// The destructive arm **without** re-prompting: revert the gate's flagged set and
/// return the outcome, never re-invoking the model. Equivalent to
/// [`apply_revert_retry`] with `max_retries = 0` — the safety properties (ledger
/// fence, workspace-boundary guard, honest `reverted` list) are inherited. This is
/// the live loop's increment-2a path; bumping to a real `RetryRerun` + cap is the
/// re-prompt increment.
///
/// # Errors
/// Propagates a revert I/O error from [`apply_revert_retry`].
pub async fn revert_only(
    workspace: &Path,
    surface: &RetrySurface<'_>,
    ledger: &RefCell<WriteLedger>,
    report: GateReport,
) -> anyhow::Result<RetryOutcome> {
    struct NoRerun;
    #[async_trait(?Send)]
    impl RetryRerun for NoRerun {
        async fn rerun(&mut self, _prompt: String) -> anyhow::Result<()> {
            Ok(())
        }
    }
    apply_revert_retry(workspace, surface, ledger, report, 0, &mut NoRerun).await
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

    // ── the `retry` mechanism ───────────────────────────────────────────────

    #[test]
    fn ledger_restores_an_edited_file() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("edit.py");
        std::fs::write(&f, "original\n").unwrap();
        let mut led = WriteLedger::new();
        led.note_before_write(&f); // captures "original"
        std::fs::write(&f, "fabricated\n").unwrap();
        assert!(led.revert(&f).unwrap());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "original\n");
    }

    #[test]
    fn ledger_deletes_a_newly_created_file() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("new.py");
        let mut led = WriteLedger::new();
        led.note_before_write(&f); // file absent → records None
        std::fs::write(&f, "import newt_core\n").unwrap();
        assert!(led.revert(&f).unwrap());
        assert!(!f.exists(), "a file that did not exist pre-turn is removed");
    }

    #[test]
    fn ledger_first_write_wins_across_a_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("multi.py");
        std::fs::write(&f, "pre-turn\n").unwrap();
        let mut led = WriteLedger::new();
        led.note_before_write(&f); // "pre-turn"
        std::fs::write(&f, "intermediate\n").unwrap();
        led.note_before_write(&f); // no-op — pre-turn state preserved
        std::fs::write(&f, "final\n").unwrap();
        led.revert(&f).unwrap();
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "pre-turn\n",
            "revert restores the pre-turn state, not an intermediate write"
        );
        assert_eq!(led.len(), 1);
    }

    #[test]
    fn ledger_revert_reports_untracked_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("untracked.py");
        std::fs::write(&f, "x\n").unwrap();
        let led = WriteLedger::new();
        assert!(!led.revert(&f).unwrap(), "no entry → false, no delete");
        assert!(f.exists(), "an untracked file is never silently removed");
    }

    #[test]
    fn note_before_write_leaves_an_unreadable_path_untracked() {
        // A directory at the path makes `std::fs::read` fail with a non-NotFound
        // error. That must NOT be recorded as the absent-marker `Some(None)` (which
        // would delete on revert) — the path is left untracked instead.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("not_a_file");
        std::fs::create_dir(&p).unwrap();
        let mut led = WriteLedger::new();
        led.note_before_write(&p);
        assert_eq!(led.len(), 0, "an unreadable path is not recorded");
        assert!(!led.revert(&p).unwrap(), "untracked ⇒ revert is a no-op");
        assert!(
            p.exists(),
            "revert must never remove an unreadable pre-existing path"
        );
    }

    #[test]
    fn corrective_prompt_names_files_modules_and_surface() {
        let report = GateReport {
            files: vec![FileVerdict {
                path: "examples/bad.py".into(),
                fabrications: vec![Fabrication {
                    module: "newt_core".into(),
                    line: 3,
                }],
            }],
        };
        let p = corrective_prompt(&report, "SURFACE-BLOCK-HERE");
        assert!(p.contains("examples/bad.py"));
        assert!(p.contains("newt_core"));
        assert!(p.contains("line 3"));
        assert!(p.contains("SURFACE-BLOCK-HERE"));
        assert!(p.contains("Do not import"));
    }

    /// A scripted re-run that mimics the live tool seam: each call writes the next
    /// queued content to `file`, recording the write in the shared ledger first.
    struct ScriptedRerun<'a> {
        workspace: PathBuf,
        file: PathBuf,
        ledger: &'a RefCell<WriteLedger>,
        contents: std::collections::VecDeque<String>,
        calls: usize,
    }

    #[async_trait(?Send)]
    impl RetryRerun for ScriptedRerun<'_> {
        async fn rerun(&mut self, _prompt: String) -> anyhow::Result<()> {
            self.calls += 1;
            let content = self.contents.pop_front().unwrap_or_default();
            let abs = self.workspace.join(&self.file);
            self.ledger.borrow_mut().note_before_write(&abs);
            std::fs::write(&abs, content)?;
            Ok(())
        }
    }

    /// Seed a one-file fabricating turn: record the pre-turn (absent) state, write
    /// the bad file, and return the initial gate report.
    fn seed_fabricating_turn(ws: &Path, file: &str, ledger: &RefCell<WriteLedger>) -> GateReport {
        let abs = ws.join(file);
        ledger.borrow_mut().note_before_write(&abs);
        std::fs::write(&abs, "import newt_core\n").unwrap();
        gate_python_workspace(ws, &surface()).unwrap()
    }

    #[tokio::test]
    async fn retry_accepts_when_a_reattempt_grounds_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = RefCell::new(WriteLedger::new());
        let initial = seed_fabricating_turn(tmp.path(), "bad.py", &ledger);
        assert!(!initial.accept());

        let mut rerun = ScriptedRerun {
            workspace: tmp.path().to_path_buf(),
            file: "bad.py".into(),
            ledger: &ledger,
            contents: ["from newt_agent._newt_agent.core import Router\n".to_string()].into(),
            calls: 0,
        };
        let surf = surface();
        let outcome = apply_revert_retry(
            tmp.path(),
            &RetrySurface {
                modules: &surf,
                mode: SurfaceMatch::Exact,
                block: "SURFACE",
            },
            &ledger,
            initial,
            2,
            &mut rerun,
        )
        .await
        .unwrap();

        assert!(outcome.accepted);
        assert_eq!(outcome.retries_used, 1);
        assert_eq!(rerun.calls, 1);
        assert!(outcome.reverted.is_empty());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("bad.py")).unwrap(),
            "from newt_agent._newt_agent.core import Router\n"
        );
    }

    #[tokio::test]
    async fn retry_gives_up_honestly_at_the_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = RefCell::new(WriteLedger::new());
        let initial = seed_fabricating_turn(tmp.path(), "bad.py", &ledger);

        // Every re-attempt keeps fabricating → never grounds.
        let mut rerun = ScriptedRerun {
            workspace: tmp.path().to_path_buf(),
            file: "bad.py".into(),
            ledger: &ledger,
            contents: ["import newt_core\n".into(), "import newt_core\n".into()].into(),
            calls: 0,
        };
        let surf = surface();
        let outcome = apply_revert_retry(
            tmp.path(),
            &RetrySurface {
                modules: &surf,
                mode: SurfaceMatch::Exact,
                block: "SURFACE",
            },
            &ledger,
            initial,
            2,
            &mut rerun,
        )
        .await
        .unwrap();

        assert!(!outcome.accepted);
        assert_eq!(
            outcome.retries_used, 2,
            "ran exactly 1 + max_retries calls' worth"
        );
        assert_eq!(rerun.calls, 2);
        assert_eq!(outcome.reverted, vec![PathBuf::from("bad.py")]);
        assert_eq!(outcome.outstanding_modules, vec!["newt_core".to_string()]);
        assert!(
            !tmp.path().join("bad.py").exists(),
            "give-up leaves the file reverted, not the last fabrication"
        );
    }

    #[tokio::test]
    async fn retry_is_a_no_op_when_the_initial_report_is_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = RefCell::new(WriteLedger::new());

        struct NeverRerun;
        #[async_trait(?Send)]
        impl RetryRerun for NeverRerun {
            async fn rerun(&mut self, _p: String) -> anyhow::Result<()> {
                panic!("rerun must not be called when the gate already accepts");
            }
        }

        let clean = GateReport {
            files: vec![FileVerdict {
                path: "ok.py".into(),
                fabrications: vec![],
            }],
        };
        let surf = surface();
        let outcome = apply_revert_retry(
            tmp.path(),
            &RetrySurface {
                modules: &surf,
                mode: SurfaceMatch::Exact,
                block: "SURFACE",
            },
            &ledger,
            clean,
            2,
            &mut NeverRerun,
        )
        .await
        .unwrap();
        assert!(outcome.accepted);
        assert_eq!(outcome.retries_used, 0);
    }

    #[tokio::test]
    async fn revert_only_touches_only_ledgered_files() {
        let tmp = tempfile::tempdir().unwrap();
        // newt creates one fabricating file (recorded in the ledger)...
        let ledger = RefCell::new(WriteLedger::new());
        let mine = tmp.path().join("mine.py");
        ledger.borrow_mut().note_before_write(&mine);
        std::fs::write(&mine, "import newt_core\n").unwrap();
        // ...and a fabricating file newt never wrote exists too (NOT in the ledger).
        let theirs = tmp.path().join("theirs.py");
        std::fs::write(&theirs, "import newt_core\n").unwrap();

        let surf = surface();
        let report = gate_python_workspace(tmp.path(), &surf).unwrap();
        assert_eq!(report.revert_set().len(), 2, "both files fabricate");

        let outcome = revert_only(
            tmp.path(),
            &RetrySurface {
                modules: &surf,
                mode: SurfaceMatch::Exact,
                block: "",
            },
            &ledger,
            report,
        )
        .await
        .unwrap();

        // Only newt's file is reverted (deleted); the other is left exactly as-is.
        assert_eq!(outcome.reverted, vec![PathBuf::from("mine.py")]);
        assert!(!mine.exists(), "newt's created fabrication is removed");
        assert_eq!(
            std::fs::read_to_string(&theirs).unwrap(),
            "import newt_core\n",
            "a file newt did not write is never touched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn gate_does_not_follow_symlinked_dirs() {
        // A symlinked directory pointing OUTSIDE the workspace must not be walked —
        // otherwise external `.py` enter the gate's blast radius.
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("external.py"), "import newt_core\n").unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("own.py"), "import newt_core\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.path().join("linked")).unwrap();

        let report = gate_python_workspace(ws.path(), &surface()).unwrap();
        let paths: Vec<_> = report.files.iter().map(|f| f.path.clone()).collect();
        assert_eq!(
            paths,
            vec![PathBuf::from("own.py")],
            "only the workspace's own .py is gated; the symlinked external file is skipped"
        );
    }
}

#[cfg(test)]
mod tier_tests {
    use super::*;

    fn dirty() -> GateReport {
        GateReport {
            files: vec![FileVerdict {
                path: PathBuf::from("fab.py"),
                fabrications: vec![Fabrication {
                    module: "newt_core".to_string(),
                    line: 1,
                }],
            }],
        }
    }

    #[test]
    fn clean_report_passes_every_tier() {
        let clean = GateReport::default();
        for t in [
            VerifyTier::Off,
            VerifyTier::Advisory,
            VerifyTier::RevertOnce,
            VerifyTier::RevertRetry,
        ] {
            assert_eq!(t.action(&clean), VerifyAction::Pass);
        }
    }

    #[test]
    fn dirty_report_maps_per_tier() {
        let d = dirty();
        assert_eq!(VerifyTier::Off.action(&d), VerifyAction::Pass);
        assert_eq!(VerifyTier::Advisory.action(&d), VerifyAction::Warn);
        assert_eq!(VerifyTier::RevertOnce.action(&d), VerifyAction::Revert);
        assert_eq!(
            VerifyTier::RevertRetry.action(&d),
            VerifyAction::RevertAndRetry
        );
    }

    #[test]
    fn default_tier_is_revert_retry() {
        assert_eq!(VerifyTier::default(), VerifyTier::RevertRetry);
        let t: VerifyTier = serde_json::from_str("\"advisory\"").unwrap();
        assert_eq!(t, VerifyTier::Advisory);
    }

    #[test]
    fn banner_is_none_on_clean_finish() {
        assert!(turn_verdict_banner(&[], false, false).is_none());
    }

    #[test]
    fn banner_names_reverts_only() {
        let b = turn_verdict_banner(&["a.py".to_string()], false, false).unwrap();
        assert!(b.contains("reverted 1 file") && b.contains("a.py"));
        assert!(!b.contains("tool-round cap"));
        assert!(b.contains("needs human review"));
    }

    #[test]
    fn banner_names_cap_only() {
        let b = turn_verdict_banner(&[], false, true).unwrap();
        assert!(b.contains("tool-round cap"));
        assert!(!b.contains("reverted"));
    }

    #[test]
    fn banner_names_both_in_one_line() {
        let b = turn_verdict_banner(&["a.py".to_string(), "b.py".to_string()], true, true).unwrap();
        assert!(b.contains("reverted 2 file"));
        assert!(b.contains("exhausting retries"));
        assert!(b.contains("tool-round cap"));
        assert_eq!(b.lines().count(), 1, "one honest line");
    }
}
