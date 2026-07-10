//! Danger-tiering for permission grants — facade **P1b** (§7-F3/F4 of
//! `docs/ocap/permissions-facade-design.md`).
//!
//! Absent an active `/mode`, the only authority boundary is the human operator,
//! and the **model controls the prompt text**: `request_permissions{capability,
//! target, reason}` carries a model-authored `reason`. A benign reason can dress
//! a catastrophic grant (`exec bash`, `fs_write /`) — a
//! confused-deputy-through-the-operator (§7-F4). This module gives the prompt a
//! fact the model cannot forge: a [`DangerTier`] computed by the harness from
//! `(capability, target)`, used to (a) render a system-computed blast-radius
//! line and (b) refuse a plain `[s]ession allow` for the catastrophic targets.
//!
//! ## Three-Cs: the tier table is DATA, not `match` arms
//!
//! Per the repo's language-pack / lexicon convention (`CLAUDE.md` → "the three
//! Cs"), the knowledge of *which* targets are dangerous lives in pure data —
//! the [`INTERPRETER_EXEC`] slice and the [`DangerTable`]'s broad-fs-root set —
//! not in hardcoded `match` arms. [`DangerTable::classify`] is a pure reader
//! over that data, so a new interpreter or a new broad root is a data edit (and
//! a future drop-in `[tui.permissions]` override), never a logic change.
//!
//! ## What "high-danger" means operationally
//!
//! A high-danger grant is **not settable from a plain `[s]ession allow`** — one
//! sticky grant of an interpreter is a standing arbitrary-code permit whose
//! `exec`'d children fork outside the per-spawn interceptor (§7-F3), and one
//! sticky grant of a broad fs root is a whole-tree permit. The operator may
//! `[a]llow once` per op or `[d]eny`; `[k]ey allow` (step-up, **P3, unbuilt**)
//! is the intended standing path for these.

use newt_core::DenialKind;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// How catastrophic an *allow* of a `(capability, target)` grant would be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DangerTier {
    /// A narrow grant — one command, one path, one host. Session-allowable.
    Low,
    /// An interpreter/shell exec name, or a broad fs root (`/`, the home dir,
    /// the workspace root). NOT settable from a plain `[s]ession allow` —
    /// allow-once only (`[k]ey allow` step-up, P3, is the future standing path).
    High,
}

/// Interpreter / shell / command-runner names whose `exec` grant is effectively
/// **arbitrary command execution**: the process they launch forks children
/// outside the per-spawn interceptor (§7-F3), so "allow `bash`" is nothing like
/// "allow `ls`".
///
/// Pure DATA (three-Cs) — extend this slice for a new interpreter without
/// touching [`DangerTable::classify`]. The trailing entries (`env`, `xargs`, …)
/// are command-runners that exec an arbitrary child; they carry the same blast
/// radius as a shell. A drop-in `[tui.permissions]` override is the follow-up.
const INTERPRETER_EXEC: &[&str] = &[
    // POSIX & common interactive shells.
    "sh", "bash", "dash", "zsh", "fish", "ksh", "ash", "tcsh", "csh",
    // Language interpreters / REPLs.
    "python", "python2", "python3", "node", "deno", "bun", "perl", "ruby", "php", "lua", "tclsh",
    "Rscript", "groovy", "irb", "pwsh",
    // Text processors that run arbitrary code (`awk` via `system()`, …).
    "awk", "gawk", "mawk",
    // Command-runners / arg-to-command wrappers: they exec an arbitrary child.
    "env", "xargs", "nohup", "nice", "timeout", "stdbuf", "setsid", "watch",
];

/// The danger-tier table — pure DATA, read by [`DangerTable::classify`].
///
/// `interpreters` is the [`INTERPRETER_EXEC`] set; `broad_fs_roots` is the set
/// of paths a grant must never reach **at or above** (the filesystem root `/`
/// always, plus the environment's home dir / workspace root added with
/// [`DangerTable::with_fs_root`]).
#[derive(Debug, Clone)]
pub struct DangerTable {
    interpreters: BTreeSet<String>,
    broad_fs_roots: Vec<PathBuf>,
}

impl DangerTable {
    /// The built-in table: the [`INTERPRETER_EXEC`] set plus the filesystem
    /// root `/` (always broad). Environment-specific broad roots (the home dir,
    /// the workspace root) are layered on with [`DangerTable::with_fs_root`].
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            interpreters: INTERPRETER_EXEC.iter().map(|s| (*s).to_string()).collect(),
            broad_fs_roots: vec![PathBuf::from("/")],
        }
    }

    /// Add an environment-specific broad fs root (the home dir, the workspace
    /// root). A grant **at or above** any broad root is high-danger, so an
    /// ancestor of a root (e.g. `/home` above `$HOME`) is broad too. Empty
    /// paths and duplicates are folded out. Builder-style so the production
    /// table is *composed* from data (three-Cs).
    #[must_use]
    pub fn with_fs_root(mut self, root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        if !root.as_os_str().is_empty() && !self.broad_fs_roots.iter().any(|r| r == root) {
            self.broad_fs_roots.push(root.to_path_buf());
        }
        self
    }

    /// Classify a `(capability, target)` grant. Pure — no fs, no env, no I/O.
    #[must_use]
    pub fn classify(&self, kind: DenialKind, target: &str) -> DangerTier {
        let high = match kind {
            DenialKind::Exec => self.is_interpreter(target),
            DenialKind::FsRead | DenialKind::FsWrite => self.is_broad_path(target),
            // A net allowlist entry is a single host — narrow by construction.
            // No net target is danger-tiered today; net is always Low.
            DenialKind::Net => false,
            // FR-2 (#1001): a remote-tool grant is Low so a coach can `[s]ession
            // allow` a remote tool it needs (a High tier would refuse the session
            // grant). The absolute-catastrophe floor is FR-3's deny-list, not this.
            DenialKind::RemoteTool => false,
            // #1056: a LOCAL git write is Low — a commit is not an interpreter or
            // a broad-tree grant, and it must be `[s]ession allow`-able so a coder
            // can commit its work without a prompt per op. The floor that still
            // denies it under a readonly `/mode` is the gate's preset check, not a
            // High tier (which would only block the session grant, not once).
            DenialKind::GitWrite => false,
        };
        if high {
            DangerTier::High
        } else {
            DangerTier::Low
        }
    }

    /// The **system-computed blast-radius line** for a high-danger grant — a
    /// fact derived from `(kind, target)` that the model cannot author (§7-F4).
    /// `None` for a low-danger grant (no banner is shown). The `⚠` framing
    /// matches the denial formatters.
    #[must_use]
    pub fn blast_radius(&self, kind: DenialKind, target: &str) -> Option<String> {
        if self.classify(kind, target) != DangerTier::High {
            return None;
        }
        let t = target.trim();
        match kind {
            DenialKind::Exec => Some(format!(
                "⚠ `{t}` is an interpreter: this grants arbitrary command execution"
            )),
            DenialKind::FsRead | DenialKind::FsWrite => {
                let verb = if kind == DenialKind::FsWrite {
                    "write"
                } else {
                    "read"
                };
                if Path::new(t) == Path::new("/") {
                    Some(format!(
                        "⚠ `/` is the filesystem root: this grants {verb} access to everything"
                    ))
                } else {
                    Some(format!(
                        "⚠ `{t}` is a broad filesystem root: this grants {verb} \
                         access to everything beneath it"
                    ))
                }
            }
            // Net, RemoteTool, and GitWrite are never high-danger (guarded by
            // `classify` above); unreachable in practice, but keep the match total.
            DenialKind::Net | DenialKind::RemoteTool | DenialKind::GitWrite => None,
        }
    }

    /// Is `target` an interpreter/shell exec name? Compares the command's
    /// file-name component, so `/usr/bin/python3` and `python3` both match.
    fn is_interpreter(&self, target: &str) -> bool {
        let trimmed = target.trim();
        let name = Path::new(trimmed)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(trimmed);
        self.interpreters.contains(name)
    }

    /// Is `target` a broad fs path — **at or above** any broad root? Granting an
    /// ancestor of a broad root (e.g. `/home` above `$HOME`) is also broad; a
    /// descendant (a real working subdir) is narrow. Component-wise via
    /// [`Path::starts_with`], so `/ho` never matches `/home`.
    fn is_broad_path(&self, target: &str) -> bool {
        let t = target.trim();
        if t.is_empty() {
            return false;
        }
        let tp = Path::new(t);
        self.broad_fs_roots
            .iter()
            .any(|root| root.as_path() == tp || root.starts_with(tp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pure-classifier contract (§7-F3/F4): interpreter exec names and broad
    /// fs roots are high-danger; a narrow command / path is low. Data-driven —
    /// the truth lives in [`INTERPRETER_EXEC`] + the table's broad-root set.
    #[test]
    fn classify_flags_interpreters_and_broad_roots_high_and_narrow_low() {
        let t = DangerTable::builtin()
            .with_fs_root("/home/me")
            .with_fs_root("/home/me/project");

        // Interpreters / shells → High (the spec's named set).
        for cmd in [
            "bash", "sh", "zsh", "fish", "python", "python3", "node", "perl", "ruby",
        ] {
            assert_eq!(
                t.classify(DenialKind::Exec, cmd),
                DangerTier::High,
                "`{cmd}` is an interpreter — high-danger"
            );
        }
        // Command-runners (exec an arbitrary child) → High.
        assert_eq!(t.classify(DenialKind::Exec, "env"), DangerTier::High);
        assert_eq!(t.classify(DenialKind::Exec, "xargs"), DangerTier::High);
        // A pathful interpreter still matches on its file-name component.
        assert_eq!(
            t.classify(DenialKind::Exec, "/usr/bin/python3"),
            DangerTier::High
        );
        // A narrow command → Low.
        for cmd in ["ls", "cargo", "git", "rg", "rm"] {
            assert_eq!(
                t.classify(DenialKind::Exec, cmd),
                DangerTier::Low,
                "`{cmd}` is a narrow command — low-danger"
            );
        }

        // Broad fs roots → High (`/`, the home dir, the workspace root, and an
        // ancestor of a root such as `/home`).
        for p in ["/", "/home/me", "/home/me/project", "/home"] {
            assert_eq!(
                t.classify(DenialKind::FsWrite, p),
                DangerTier::High,
                "`{p}` is a broad fs root — high-danger"
            );
            assert_eq!(t.classify(DenialKind::FsRead, p), DangerTier::High);
        }
        // A real working subdir → Low.
        for p in [
            "/home/me/project/src",
            "/home/me/project/Cargo.toml",
            "/tmp/scratch",
        ] {
            assert_eq!(
                t.classify(DenialKind::FsWrite, p),
                DangerTier::Low,
                "`{p}` is a narrow path — low-danger"
            );
        }

        // Net is never danger-tiered.
        assert_eq!(t.classify(DenialKind::Net, "example.com"), DangerTier::Low);
    }

    /// The blast-radius line is computed by the harness from `(kind, target)` —
    /// `Some` only for a high-danger grant, with the verb keyed to the axis.
    #[test]
    fn blast_radius_is_system_computed_for_high_only() {
        let t = DangerTable::builtin().with_fs_root("/home/me/project");

        let exec = t.blast_radius(DenialKind::Exec, "bash").unwrap();
        assert!(exec.contains("interpreter"), "got: {exec}");
        assert!(exec.contains("arbitrary command execution"), "got: {exec}");

        let root = t.blast_radius(DenialKind::FsWrite, "/").unwrap();
        assert!(root.contains("filesystem root"), "got: {root}");
        assert!(root.contains("write access to everything"), "got: {root}");

        let read_root = t.blast_radius(DenialKind::FsRead, "/").unwrap();
        assert!(
            read_root.contains("read access to everything"),
            "got: {read_root}"
        );

        let ws = t
            .blast_radius(DenialKind::FsWrite, "/home/me/project")
            .unwrap();
        assert!(ws.contains("broad filesystem root"), "got: {ws}");

        // No banner for a low-danger grant.
        assert!(t.blast_radius(DenialKind::Exec, "ls").is_none());
        assert!(t
            .blast_radius(DenialKind::FsWrite, "/home/me/project/src/main.rs")
            .is_none());
        assert!(t.blast_radius(DenialKind::Net, "example.com").is_none());
    }

    /// `with_fs_root` folds out empties and duplicates, and a descendant of an
    /// existing root does not widen the set (it is already covered narrowly).
    #[test]
    fn with_fs_root_is_idempotent_on_empties_and_dupes() {
        let a = DangerTable::builtin()
            .with_fs_root("")
            .with_fs_root("/home/me");
        let b = a.clone().with_fs_root("/home/me");
        // The empty root was dropped; the duplicate did not re-add.
        assert_eq!(b.broad_fs_roots.len(), a.broad_fs_roots.len());
        // `/` is always present from `builtin`.
        assert!(b
            .broad_fs_roots
            .iter()
            .any(|r| r.as_path() == Path::new("/")));
    }
}
